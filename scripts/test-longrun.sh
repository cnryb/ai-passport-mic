#!/usr/bin/env bash
# G2 / KI-006 长测：真机跑 N 秒，判「0 欠载」并输出可粘贴的报告。
#
# 为什么单独一个脚本：G2 的判据是"连续 10 min 0 欠载"，此前每次都要人工盯界面或手敲
# `apmctl sink --seconds 600`，然后把日志里的数字抄进文档。抄错一次（例如把 159 记成 39）
# 就会让人对整条链路失去信任。这里把**跑 + 判 + 出报告**固定下来。
#
# 它验的是：链路（块率 / 序号跳变 / 设备丢弃）+ 声卡（欠载 / 静音 / 超容量 / 通道满）+ 漂移。
# 它**不验**音质、不验会议软件里能不能选中该设备。
#
# 用法:
#   ./scripts/test-longrun.sh              # 默认 600 s（G2 正式口径）
#   ./scripts/test-longrun.sh --seconds 120 --target-ms 80
#   ./scripts/test-longrun.sh --dry-run    # 只打印将要执行的命令与判据
#
# 退出码：0 = G2 PASS（0 欠载且链路干净）；1 = FAIL；2 = SKIP（环境不满足）。
# 报告同时写到 $TMPDIR 下，路径在最后一行打印。
set -uo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
seconds=600
target_ms=80
sink_name="BlackHole 2ch"
dry_run=0

while (( $# )); do
  case "$1" in
    --seconds)    seconds="${2:-}"; shift 2 ;;
    --target-ms)  target_ms="${2:-}"; shift 2 ;;
    --sink)       sink_name="${2:-}"; shift 2 ;;
    --dry-run)    dry_run=1; shift ;;
    -h|--help)    sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "未知参数: $1" >&2; exit 2 ;;
  esac
done

if [[ ! "$seconds" =~ ^[0-9]+$ ]] || (( seconds < 10 )); then
  echo "FAIL --seconds 必须是 ≥10 的整数（拿到 '$seconds'）" >&2
  exit 2
fi

log="${TMPDIR:-/tmp}/apm-longrun-${seconds}s-$(date +%Y%m%d-%H%M%S).log"

# --- 环境 ---
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

cmd=("$root/scripts/apmctl-macos.sh" sink --to "$sink_name" \
     --seconds "$seconds" --target-ms "$target_ms" --log "$log")

echo "== G2 / KI-006 长测 =="
echo "时长   : ${seconds}s"
echo "目标   : ${target_ms} ms"
echo "声卡   : ${sink_name}"
echo "报告   : $log"
echo

if (( dry_run )); then
  echo "DRY-RUN 将执行："
  printf '  %q ' "${cmd[@]}"; echo
  echo
  echo "判据（全部满足才算 G2 PASS）："
  echo "  1. 块率 50.000/s（±0.5%）"
  echo "  2. 欠载 = 0"
  echo "  3. 序号跳变丢失 = 0、长度异常 = 0、设备侧丢弃 = 0"
  echo "  4. 超容量丢弃 = 0、通道满丢块 = 0"
  exit 0
fi

# 跑。apmctl 经 LaunchServices 启动拿不到 stdout，只能靠 --log 回传，
# 所以这里的 stdout 只是包装层的输出，判据一律从日志里读。
"${cmd[@]}" || true

if [[ ! -s "$log" ]]; then
  echo
  echo "FAIL 日志为空，apmctl 可能没起来（先单独跑一次 scan 验蓝牙权限）"
  exit 1
fi

echo
echo "== 判据 =="

# 收尾摘要那几行是 apmctl 自己算的权威数字，优先用它们；缺失才回退到逐秒采样。
blocks=$(sed -n 's/^块: \([0-9]*\) 个.*/\1/p' "$log" | tail -1)
jump_loss=$(sed -n 's/.*序号跳变丢失: \([0-9]*\)（跳变 \([0-9]*\) 次）.*/\1/p' "$log" | tail -1)
bad_len=$(sed -n 's/.*长度异常: \([0-9]*\).*/\1/p' "$log" | tail -1)
dev_drop=$(sed -n 's/.*设备侧丢弃: \([0-9]*\).*/\1/p' "$log" | tail -1)
played=$(sed -n 's/^声卡: 已播 \([0-9.]*\) s.*/\1/p' "$log" | tail -1)
underruns=$(sed -n 's/.*欠载 \([0-9]*\) 次.*/\1/p' "$log" | tail -1)
overflow=$(sed -n 's/.*超容量丢弃 \([0-9]*\) 帧.*/\1/p' "$log" | tail -1)
trimmed=$(sed -n 's/.*停摆修剪 \([0-9]*\) 帧.*/\1/p' "$log" | tail -1)
chan_full=$(sed -n 's/^.*通道满丢块 \([0-9]*\)/\1/p' "$log" | tail -1)
lockmiss=$(sed -n 's/.*回调抢锁失败 \([0-9]*\) 次.*/\1/p' "$log" | tail -1)
ppm=$(sed -n 's/^漂移: \([-0-9]*\) ppm.*/\1/p' "$log" | tail -1)

say() { printf '  %-22s %s\n' "$1" "$2"; }

say "块数"       "${blocks:-?}"
say "已播"       "${played:-?} s"
say "序号跳变丢失" "${jump_loss:-?}"
say "长度异常"    "${bad_len:-?}"
say "设备侧丢弃"  "${dev_drop:-?}"
say "欠载"       "${underruns:-?} 次"
say "超容量丢弃"  "${overflow:-?} 帧"
say "停摆修剪"    "${trimmed:-?} 帧"
say "通道满丢块"  "${chan_full:-?}"
say "抢锁失败"    "${lockmiss:-?} 次"
say "报告漂移"    "${ppm:-?} ppm"

# 块率：用块数 / 已播秒数，两个都是 apmctl 的收尾统计。
rate_verdict="SKIP"
if [[ -n "${blocks:-}" && -n "${played:-}" ]] && \
   awk -v b="$blocks" -v s="$played" 'BEGIN{exit !(s>0)}'; then
  rate=$(awk -v b="$blocks" -v s="$played" 'BEGIN{printf "%.3f", b/s}')
  say "块率" "$rate /s（判据 50.000 ±0.5%）"
  rate_verdict=$(awk -v r="$rate" 'BEGIN{exit !(r>=49.75 && r<=50.25)}' && echo PASS || echo FAIL)
  say "块率判定" "$rate_verdict"
fi

verdict() { # <值> <期望>
  [[ "${1:-}" == "$2" ]] && echo PASS || echo FAIL
}

u_v=$(verdict "${underruns:-x}" 0)
j_v=$(verdict "${jump_loss:-x}" 0)
b_v=$(verdict "${bad_len:-x}" 0)
d_v=$(verdict "${dev_drop:-x}" 0)
o_v=$(verdict "${overflow:-x}" 0)
c_v=$(verdict "${chan_full:-x}" 0)

echo
echo "== 结论 =="
printf '  %-24s %s\n' "块率 50/s"      "$rate_verdict"
printf '  %-24s %s\n' "欠载 0"         "$u_v"
printf '  %-24s %s\n' "序号跳变丢失 0" "$j_v"
printf '  %-24s %s\n' "长度异常 0"     "$b_v"
printf '  %-24s %s\n' "设备侧丢弃 0"   "$d_v"
printf '  %-24s %s\n' "超容量丢弃 0"   "$o_v"
printf '  %-24s %s\n' "通道满丢块 0"   "$c_v"

echo
if [[ "$rate_verdict" == PASS && "$u_v" == PASS && "$j_v" == PASS && "$b_v" == PASS \
      && "$d_v" == PASS && "$o_v" == PASS && "$c_v" == PASS ]]; then
  echo "G2: PASS（连续 ${seconds}s 0 欠载，链路干净）"
  echo "注意：这只覆盖「写进虚拟声卡不欠载」，不含音质与会议软件的设备选择。"
  echo "报告: $log"
  exit 0
fi

echo "G2: FAIL"
echo "报告: $log"
if [[ "${underruns:-x}" != 0 ]]; then
  echo "欠载非 0 → 见 docs/KNOWN_ISSUES.md KI-006"
  # 把根因证据也打出来，否则下一个人又得从"消费端停摆"这个已被推翻的旧结论重新猜。
  if grep -q "块到达间隔" "$log"; then
    echo
    grep -E "块到达间隔|到达间隔分布|判定: 有 " "$log" | sed 's/^/  /'
    echo "  根因（2026-11-07 定位）：链路到达抖动突破 80 ms 缓冲上限，不是声卡问题。"
    echo "  修法三选一（抬上限 / 改连接参数 / 抖动感知自适应）见 docs/TODO.md 的决策表。"
  else
    echo "  本次日志没有到达间隔统计（可能是旧二进制），重跑 cargo build --release"
  fi
fi
exit 1
