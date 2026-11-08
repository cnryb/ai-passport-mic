#!/usr/bin/env bash
# G2 / KI-006 长测：真机跑 N 轮，按**多轮中位数**判欠载，并输出可粘贴的报告。
#
# 为什么单独一个脚本：G2 的判据历经"0 欠载"→"阈值型"，此前每次都要人工盯界面或手敲
# `apmctl sink --seconds 600`，然后把日志里的数字抄进文档。抄错一次（例如把 159 记成 39）
# 就会让人对整条链路失去信任。这里把**跑 + 判 + 出报告**固定下来。
#
# 为什么改成多轮中位数（2026-11-07 晚）：
#   欠载次数**运行间方差极大**是已记录的事实（KI-006 §"运行间方差极大"：
#   同样条件 60 s 欠载 1 次、180 s 5 次、600 s **49 次**；尖峰 90 → 387 ms）。
#   用单轮结果判 PASS/FAIL 等于掷骰子 —— 2026-11-07 下午连跑 7 轮得到
#   3.5 / 4.0 / 6.5 / 6.9 / 7.5 / 8.1 / 8.5 次/分，同一固件同样条件。
#   单轮阈值的成败取决于运气，而不取决于质量。
#   改成跑 3 轮取中位数之后，比较的是"这个版本在这一天的典型表现"，
#   版本间对比才有意义。
#
# 它验的是：链路（块率 / 序号跳变 / 设备丢弃）+ 声卡（欠载 / 静音 / 超容量 / 通道满）+ 漂移。
# 它**不验**音质、不验会议软件里能不能选中该设备。
#
# 用法:
#   ./scripts/test-longrun.sh                      # 默认 3 轮 × 600 s（G2 正式口径）
#   ./scripts/test-longrun.sh --rounds 1 --seconds 120   # 快速冒烟（单轮，不用于验收）
#   ./scripts/test-longrun.sh --dry-run            # 只打印将要执行的命令与判据
#   ./scripts/test-longrun.sh --from-logs "logs/*.log"   # 不重跑，只判读已有日志
#
# --from-logs 的用途：同一批日志换判据复核（例如阈值调整后回看历史跑分），
# 不必再等 30 分钟重跑。也用于对判据本身做离线验证。
#
# 退出码：0 = G2 PASS；1 = FAIL；2 = SKIP（环境不满足）。
# 报告同时写到 $TMPDIR 下，路径在最后一行打印。
set -uo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
seconds=600
rounds=3
target_ms=80
sink_name="BlackHole 2ch"
dry_run=0
from_logs=""

while (( $# )); do
  case "$1" in
    --seconds)    seconds="${2:-}"; shift 2 ;;
    --rounds)     rounds="${2:-}"; shift 2 ;;
    --target-ms)  target_ms="${2:-}"; shift 2 ;;
    --sink)       sink_name="${2:-}"; shift 2 ;;
    --from-logs)  from_logs="${2:-}"; shift 2 ;;
    --dry-run)    dry_run=1; shift ;;
    -h|--help)    sed -n '2,45p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "未知参数: $1" >&2; exit 2 ;;
  esac
done

if [[ ! "$seconds" =~ ^[0-9]+$ ]] || (( seconds < 10 )); then
  echo "FAIL --seconds 必须是 ≥10 的整数（拿到 '$seconds'）" >&2
  exit 2
fi
if [[ ! "$rounds" =~ ^[0-9]+$ ]] || (( rounds < 1 )); then
  echo "FAIL --rounds 必须是 ≥1 的整数（拿到 '$rounds'）" >&2
  exit 2
fi

stamp=$(date +%Y%m%d-%H%M%S)
logdir="${TMPDIR:-/tmp}"
declare -a round_logs=()

# --- 环境 ---
# --from-logs 不连设备、不碰声卡，所以跳过这两项环境检查。
if [[ -z "$from_logs" ]]; then
  if ! command -v system_profiler >/dev/null 2>&1; then
    echo "SKIP 没有 system_profiler（不是 macOS？）"
    exit 2
  fi
  if ! system_profiler SPAudioDataType 2>/dev/null | grep -qi blackhole; then
    echo "SKIP 没找到 BlackHole 虚拟声卡。装上之后才有意义："
    echo "     brew install --cask blackhole-2ch"
    exit 2
  fi
  if [[ ! -x "$root/scripts/apmctl-macos.sh" ]]; then
    echo "FAIL 缺少 scripts/apmctl-macos.sh"
    exit 2
  fi
fi

# ---- 阈值（三个维度，任一超标即 FAIL）----
#
# 阈值本身**不随轮数变化**：多轮取中位数是为了让"这一轮恰好运气差"不再决定结论，
# 不是为了放宽标准。依据见 docs/KNOWN_ISSUES.md KI-006 的「方案 D 的阈值建议」：
#
#   频率：实测分布 1–15.9 次/分（且有过 49 次/分的单轮），取 5 作为"典型表现"的门槛；
#   单次：实测最长约 48 ms，取 50 ms；
#   占比：实测约 0.35%，取 1%。
#
# 注意频率阈值的局限：它是分布中段而不是上界。单轮超过 5 **不代表**退化，
# 只有**中位数**超过 5 才算。脚本会同时打印每一轮的值与中位数，别只看结论那一行。
UNDERRUN_PER_MIN_MAX="${UNDERRUN_PER_MIN_MAX:-5}"
UNDERRUN_SINGLE_MS_MAX="${UNDERRUN_SINGLE_MS_MAX:-50}"
SILENCE_RATIO_PCT_MAX="${SILENCE_RATIO_PCT_MAX:-1.0}"

if (( dry_run )); then
  echo "== G2 / KI-006 长测（DRY-RUN）=="
  echo "轮数   : ${rounds} × ${seconds}s"
  echo "目标   : ${target_ms} ms"
  echo "声卡   : ${sink_name}"
  echo
  echo "每轮将执行："
  printf '  %q ' "$root/scripts/apmctl-macos.sh" sink --to "$sink_name" \
        --seconds "$seconds" --target-ms "$target_ms" --rtt --log "<log>"
  echo; echo
  echo "判据（全部满足才算 G2 PASS）："
  echo "  1. 块率 50.000/s（±0.5%）—— 每轮都必须满足"
  echo "  2. 欠载（阈值型，多轮取中位数）："
  echo "       频率中位 ≤ ${UNDERRUN_PER_MIN_MAX} 次/分 · 单次中位 ≤ ${UNDERRUN_SINGLE_MS_MAX} ms · 占比中位 ≤ ${SILENCE_RATIO_PCT_MAX}%"
  echo "     （单一维度用中位数；单轮超标不算退化，中位数超标才算）"
  echo "  3. 序号跳变丢失 = 0、长度异常 = 0、设备侧丢弃 = 0 —— **每一轮**都必须为 0"
  echo "  4. 超容量丢弃 = 0、通道满丢块 = 0 —— **每一轮**都必须为 0"
  echo
  echo "为什么第 2 项用中位数而第 3/4 项要求每轮为 0："
  echo "  欠载来自主机攒批（已知机制、方差极大），用中位数看典型表现；"
  echo "  丢块/跳变/溢出是真丢数据，任何一轮出现都是缺陷，不能用中位数稀释。"
  exit 0
fi

# 从一份日志里提取一个字段。字段缺失时返回空串（调用方用 :-? 显示）。
pick() { sed -n "$1" "$2" | tail -1; }

# 升序数组的中位数（偶数取中间两个的平均）。空数组返回空。
#
# 不用 `mapfile`：macOS 自带的是 bash 3.2，没有这个内建命令
# （2026-11-07 实测报 `mapfile: command not found`）。本脚本要能在默认 shell 下跑，
# 所以只用 3.2 就有的东西：数组、`sort`、命令替换。
median_of() {
  local n=$#
  (( n == 0 )) && { echo ""; return; }
  local sorted
  sorted=$(printf '%s\n' "$@" | sort -g)
  if (( n % 2 == 1 )); then
    printf '%s\n' "$sorted" | sed -n "$(((n + 1) / 2))p"
  else
    local a b
    a=$(printf '%s\n' "$sorted" | sed -n "$((n / 2))p")
    b=$(printf '%s\n' "$sorted" | sed -n "$((n / 2 + 1))p")
    awk -v x="$a" -v y="$b" 'BEGIN{printf "%.2f", (x+y)/2}'
  fi
}

echo "== G2 / KI-006 长测 =="
if [[ -n "$from_logs" ]]; then
  echo "模式   : 判读已有日志（不重跑）"
  echo "日志   : $from_logs"
else
  echo "轮数   : ${rounds} × ${seconds}s（共 ${rounds} 轮，取中位数）"
  echo "目标   : ${target_ms} ms"
  echo "声卡   : ${sink_name}"
fi
echo

# ---- 逐轮跑 ----
declare -a r_rate=() r_single=() r_ratio=() r_blocks=()
declare -a r_jump=() r_badlen=() r_devdrop=() r_overflow=() r_chanfull=() r_blockrate=()
declare -a r_rtt=() r_delivery=()
dirty_rounds=()

if [[ -n "$from_logs" ]]; then
  # 展开 glob（不加引号才有意义）。按文件名排序，保证轮次顺序稳定。
  # 同样避开 `mapfile`（bash 3.2 没有）。
  shopt -s nullglob
  # shellcheck disable=SC2206
  _matched=( $from_logs )
  shopt -u nullglob
  if (( ${#_matched[@]} == 0 )); then
    echo "FAIL --from-logs 没有匹配到任何文件：$from_logs"
    exit 2
  fi
  while IFS= read -r _line; do
    round_logs+=("$_line")
  done < <(printf '%s\n' "${_matched[@]}" | sort)
  rounds=${#round_logs[@]}
fi

for (( i = 1; i <= rounds; i++ )); do
  if [[ -n "$from_logs" ]]; then
    log="${round_logs[$((i - 1))]}"
  else
    log="$logdir/apm-longrun-${seconds}s-r${i}of${rounds}-${stamp}.log"
    round_logs+=("$log")
  fi
  echo "-- 第 ${i}/${rounds} 轮 --"

  if [[ -z "$from_logs" ]]; then
    "$root/scripts/apmctl-macos.sh" sink --to "$sink_name" \
        --seconds "$seconds" --target-ms "$target_ms" --rtt --log "$log" >/dev/null 2>&1 || true
  fi

  if [[ ! -s "$log" ]]; then
    echo "   FAIL 日志为空或不存在：$log"
    exit 1
  fi

  blocks=$(pick 's/^块: \([0-9]*\) 个.*/\1/p' "$log")
  played=$(pick 's/^声卡: 已播 \([0-9.]*\) s.*/\1/p' "$log")
  underruns=$(pick 's/.*欠载 \([0-9]*\) 次.*/\1/p' "$log")
  silent_frames=$(pick 's/.*静音 \([0-9]*\) 帧.*/\1/p' "$log")
  jump=$(pick 's/.*序号跳变丢失: \([0-9]*\)（跳变 \([0-9]*\) 次）.*/\1/p' "$log")
  badlen=$(pick 's/.*长度异常: \([0-9]*\).*/\1/p' "$log")
  devdrop=$(pick 's/.*设备侧丢弃: \([0-9]*\).*/\1/p' "$log")
  overflow=$(pick 's/.*超容量丢弃 \([0-9]*\) 帧.*/\1/p' "$log")
  chanfull=$(pick 's/^.*通道满丢块 \([0-9]*\)/\1/p' "$log")
  rtt=$(pick 's/^链路 RTT: [0-9]* 个采样，中位 \([0-9.]*\) ms.*/\1/p' "$log")
  delivery=$(pick 's/^通知交付模式: \([a-z]*\).*/\1/p' "$log")

  # 本项目的音频链路固定 16 kHz 单声道（`apm_protocol::AUDIO_SAMPLE_RATE`），
  # 不跟随声卡 —— 写死是对的。
  aud_rate=16000
  rate=$(awk -v u="${underruns:-0}" -v s="${played:-0}" \
    'BEGIN{if(s+0>0) printf "%.1f", u/(s/60.0); else print ""}')
  single=$(awk -v f="${silent_frames:-0}" -v u="${underruns:-0}" -v r="$aud_rate" \
    'BEGIN{if(u+0>0) printf "%.1f", (f/u)/r*1000; else print 0}')
  ratio=$(awk -v f="${silent_frames:-0}" -v r="$aud_rate" -v s="${played:-0}" \
    'BEGIN{if(s+0>0) printf "%.2f", (f/r)/s*100; else print 0}')
  blk_rate=$(awk -v b="${blocks:-0}" -v s="${played:-0}" \
    'BEGIN{if(s+0>0) printf "%.3f", b/s; else print ""}')

  r_rate+=("${rate:-0}"); r_single+=("${single:-0}"); r_ratio+=("${ratio:-0}")
  r_blocks+=("${blocks:-0}")
  r_jump+=("${jump:-?}"); r_badlen+=("${badlen:-?}"); r_devdrop+=("${devdrop:-?}")
  r_overflow+=("${overflow:-?}"); r_chanfull+=("${chanfull:-?}")
  r_blockrate+=("${blk_rate:-0}")
  r_rtt+=("${rtt:-?}"); r_delivery+=("${delivery:-?}")

  echo "   块率 ${blk_rate:-?}/s · 欠载 ${rate:-?} 次/分 · 单次 ${single:-?} ms · 占比 ${ratio:-?}%"
  echo "   跳变 ${jump:-?} · 长度异常 ${badlen:-?} · 设备丢弃 ${devdrop:-?} · 超容量 ${overflow:-?} · 通道满 ${chanfull:-?}"
  echo "   交付 ${delivery:-?} · RTT 中位 ${rtt:-?} ms"
  echo "   报告 $log"

  # 链路干净性：任何一轮非 0 都记为脏（不参与中位数稀释）。
  if [[ "${jump:-x}" != "0" || "${badlen:-x}" != "0" || "${devdrop:-x}" != "0" \
        || "${overflow:-x}" != "0" || "${chanfull:-x}" != "0" ]]; then
    dirty_rounds+=("$i")
  fi
  echo
done

# ---- 汇总 ----
med_rate=$(median_of "${r_rate[@]}")
med_single=$(median_of "${r_single[@]}")
med_ratio=$(median_of "${r_ratio[@]}")
med_blockrate=$(median_of "${r_blockrate[@]}")

echo "== 逐轮明细 =="
printf '  %-6s %-10s %-12s %-11s %-9s %-10s\n' \
       "轮次" "块率/s" "欠载次/分" "单次ms" "占比%" "链路"
for (( i = 0; i < rounds; i++ )); do
  link="干净"
  [[ " ${dirty_rounds[*]:-} " == *" $((i + 1)) "* ]] && link="脏"
  printf '  %-6s %-10s %-12s %-11s %-9s %-10s\n' \
         "$((i + 1))" "${r_blockrate[$i]:-?}" "${r_rate[$i]:-?}" \
         "${r_single[$i]:-?}" "${r_ratio[$i]:-?}" "$link"
done
if (( rounds > 1 )); then
  printf '  %-6s %-10s %-12s %-11s %-9s\n' "中位" "$med_blockrate" "$med_rate" "$med_single" "$med_ratio"
fi
echo

# 块率：每轮都要在 49.75–50.25 之间（这是恒定的，不受方差影响）。
rate_v="PASS"
for v in "${r_blockrate[@]}"; do
  awk -v r="$v" 'BEGIN{exit !(r>=49.75 && r<=50.25)}' || rate_v="FAIL"
done
# 链路干净：每轮都必须 0。
dirty_v="PASS"; (( ${#dirty_rounds[@]} > 0 )) && dirty_v="FAIL"
# 欠载：看中位数。
u_v=$(awk -v a="$med_rate" -v b="$med_single" -v c="$med_ratio" \
      -v A="$UNDERRUN_PER_MIN_MAX" -v B="$UNDERRUN_SINGLE_MS_MAX" -v C="$SILENCE_RATIO_PCT_MAX" \
      'BEGIN{exit !(a+0<=A+0 && b+0<=B+0 && c+0<=C+0)}' && echo PASS || echo FAIL)

echo "== 结论 =="
printf '  %-26s %s\n' "块率 50/s（每轮）"     "$rate_v（中位 ${med_blockrate}/s）"
printf '  %-26s %s\n' "欠载（中位数）"        "$u_v（${med_rate} 次/分 / 单次 ${med_single} ms / 占比 ${med_ratio}%）"
printf '  %-26s %s\n' "链路干净（每轮）"      "$dirty_v"
echo

if [[ "$rate_v" == PASS && "$u_v" == PASS && "$dirty_v" == PASS ]]; then
  echo "G2: PASS（${rounds} 轮 × ${seconds}s；欠载中位 ${med_rate} 次/分、单次 ${med_single} ms、占比 ${med_ratio}%）"
  echo "注意：这只覆盖「写进虚拟声卡」的质量，不含音质与会议软件的设备选择。"
  echo "      欠载判据用**多轮中位数**（2026-11-07 晚起）：根因是 macOS CoreBluetooth 攒批交付，"
  echo "      单轮方差极大，单轮数字不构成结论。详见 docs/KNOWN_ISSUES.md KI-006。"
  printf '报告:\n'; printf '  %s\n' "${round_logs[@]}"
  exit 0
fi

echo "G2: FAIL"
printf '报告:\n'; printf '  %s\n' "${round_logs[@]}"
echo

if [[ "$u_v" != "PASS" ]]; then
  echo "欠载超标（中位数）：${med_rate} 次/分（阈 ${UNDERRUN_PER_MIN_MAX}）"
  echo "  单次 ${med_single} ms（阈 ${UNDERRUN_SINGLE_MS_MAX}）/ 占比 ${med_ratio}%（阈 ${SILENCE_RATIO_PCT_MAX}%）"
  echo "  逐轮：${r_rate[*]}"
  echo
  echo "  判读顺序：① 交付模式是否 clumped；② 若 clumped 且 RTT 平稳 → 属于已知机制；"
  echo "           ③ 若「我们自己」那段占比大 → 查 PC 侧停顿。详见 KI-006。"
  echo "  最后一轮的三把尺子："
  last_log="${round_logs[$((${#round_logs[@]} - 1))]}"
  grep -E "链路到达间隔|PC 处理耗时|读取等待|最长间隔拆解|链路 RTT|通知交付模式" "$last_log" \
    | sed 's/^/    /'
fi

if [[ "$dirty_v" != "PASS" ]]; then
  echo "链路不干净：第 ${dirty_rounds[*]} 轮出现非 0 的跳变/长度异常/设备丢弃/超容量/通道满。"
  echo "  这五项与主机攒批无关，属于真正的丢数据 —— **不能用中位数稀释**。"
  for idx in "${dirty_rounds[@]}"; do
    lg="${round_logs[$((idx - 1))]}"
    echo "  第 $idx 轮：跳变 ${r_jump[$((idx - 1))]} / 长度异常 ${r_badlen[$((idx - 1))]} / 设备丢弃 ${r_devdrop[$((idx - 1))]} / 超容量 ${r_overflow[$((idx - 1))]} / 通道满 ${r_chanfull[$((idx - 1))]}"
    echo "    报告 $lg"
  done
fi

if [[ "$rate_v" != "PASS" ]]; then
  echo "块率不合格：逐轮 ${r_blockrate[*]}（要求每轮都在 49.75–50.25）"
fi
exit 1
