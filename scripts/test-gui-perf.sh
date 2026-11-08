#!/usr/bin/env bash
# G2 性能三项的可复现测量：CPU、内存、前端帧率。
#
# 存在的理由：2026-11-05 那次 §14.10 是**人盯活动监视器抄数字**，抄完就再也复现不了，
# 而且口径（主进程 vs 全家桶）当场就产生了分歧。这个脚本把三件事固定下来：
#
#   1. **测什么进程**：`.app` 的全部相关进程按 bundle 归属汇总（含 WebView 与 helper），
#      另外单独报主进程。两个数字都打印，让读的人自己按口径取。
#   2. **什么时候测**：先等应用起来并连上设备/开流，再进入采样窗口 ——
#      起播阶段的 CPU 峰值不代表稳态。
#   3. **判据怎么算**：CPU / 内存按 RSS 与 %CPU 的**采样均值**，不是某一瞬间的截图。
#
# **口径边界（别混用）**：这里量的是**全进程**的 CPU% 与 RSS，包含 WebKit helper，
# 是 G2 判据本身。开发时为了比对两个实现，也可以用 Chrome DevTools Protocol 的
# `Performance.getMetrics` 量渲染进程的主线程时间（TaskDuration / ScriptDuration），
# 那个数字更细但**不是同一个口径**，不能拿来判 G2 过没过。
# 2026-11-07 的电平表改造就是这么比的（见 GUI_IMPLEMENTATION_LOG.md §14.26）：
# 主线程 ScriptDuration −42%，但 G2 的 CPU/内存仍需用本脚本重测。
#
# 用法:
#   ./scripts/test-gui-perf.sh                  # 60 s 采样
#   ./scripts/test-gui-perf.sh --seconds 180
#   ./scripts/test-gui-perf.sh --app /path/to/App.app
#
# 退出码：0 = 三项都在口径内；1 = 有项超标；2 = SKIP（环境不满足）。
#
# 前提：设备已上电、在广播；虚拟声卡（BlackHole）已装。脚本会自己开流。
set -uo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
seconds=60
app="${root}/desktop/target/release/bundle/macos/AI Passport Mic.app"
warmup=15

while (( $# )); do
  case "$1" in
    --seconds) seconds="${2:-}"; shift 2 ;;
    --app)     app="${2:-}";     shift 2 ;;
    --warmup)  warmup="${2:-}";  shift 2 ;;
    -h|--help) sed -n '2,24p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "未知参数: $1" >&2; exit 2 ;;
  esac
done

# --- 口径（与 docs/GUI.md 的 G2 一致；改这里要同步改文档）-------------------
CPU_MAX_PCT=8
MEM_MAX_MB=150
FPS_MIN=30

if [[ ! -d "$app" ]]; then
  echo "SKIP 没有 .app：$app"
  echo "     先跑 ./scripts/build-gui.sh --release（debug 包的数据不能当验收）"
  exit 2
fi
if [[ "$app" != *"/release/"* ]]; then
  echo "SKIP 这不是 release 包：$app"
  echo "     debug 包带边界检查、不打优化，CPU 与帧率都不能当验收数据（`GUI_IMPLEMENTATION_LOG.md` §14.10 的教训）"
  exit 2
fi
if ! system_profiler SPAudioDataType 2>/dev/null | grep -qi blackhole; then
  echo "SKIP 没找到 BlackHole 虚拟声卡"
  exit 2
fi

bundle_id="com.cnryb.ai-passport-mic"

# 该 App 的**全部相关进程**怎么识别 —— 这是本脚本最容易做错的地方。
#
# 踩过的两个坑（2026-11-07 实测）：
#
#   1. **按名字匹配会漏掉大头**。Tauri 的界面跑在 WebKit 的 XPC helper 里，
#      那些进程的 comm 是 `/System/Library/.../com.apple.WebKit.GPU.xpc/...`，
#      完全不含应用名。第一版据此量出"CPU 3.4%、内存 79 MB"——只算了主进程，
#      而真正的 CPU 大头（GPU 15.6% + WebContent 11.4%）一个都没进来。
#
#   2. **按 PID 或父子链推断归属会算错**。那些 helper 的 ppid 是 1（launchd 拉起），
#      `lsof` 也看不到它们打开应用 bundle 的文件，系统没有公开接口把 XPC 归给某个 App。
#      按"PID >= 主进程"当代理更糟：会把同时开着的 Cursor 自己的 WebKit 也算进来。
#
# 用**启动前后的 PID 差值**：应用启动前拍一次快照，启动后新增的那些就是它拉起的。
# 这个方法不依赖任何归属推断，且实测稳定（每次都是恰好 3 个 WebKit + 1 个主进程）。
# 锁定之后只采这批 PID，中途新出现的进程不计（避免把别的 App 的 WebKit 混进来）。

# 拍快照（只取 PID，按数值排序）。
snapshot_pids() {
  ps -axo pid= | tr -d ' ' | sort -n
}

# `$1` = 基准快照文件，`$2` = 当前快照文件 → 输出新增的 PID。
new_pids_since() {
  comm -13 "$1" "$2"
}

echo "== G2 性能测量（release 包）=="
echo "应用   : $app"
echo "采样   : 预热 ${warmup}s + 测量 ${seconds}s"
echo "口径   : CPU < ${CPU_MAX_PCT}%、内存 < ${MEM_MAX_MB} MB、前端 ≥ ${FPS_MIN} fps"
echo

# --- 清场：先关掉可能已经在跑的实例，避免把旧进程算进来 --------------------
osascript -e "tell application id \"${bundle_id}\" to quit" >/dev/null 2>&1 || true
sleep 2
if pgrep -f "AI Passport Mic" >/dev/null 2>&1; then
  echo "注意: 仍有 AI Passport Mic 进程在跑，先杀掉（否则统计会混入旧进程）"
  pkill -f "AI Passport Mic" || true
  sleep 1
fi

echo "== 自动开流（--smoke-perf）=="
# GUI 的无人值守入口只有 scan / firmware / perf（`GUI_IMPLEMENTATION_LOG.md` §14.5：LaunchServices 不收 --args）。
# perf 入口会连上设备、开流、保持 N 秒 —— 正好覆盖我们要采样的稳态窗口。
# 用哨兵文件传参（路径与 `apm_app::settings::data_dir()` 一致）。
data_dir="${HOME}/Library/Application Support/com.cnryb.ai-passport-mic"
mkdir -p "$data_dir"
# 应用内保持推流的秒数必须**覆盖**脚本的整个采样窗口，再留余量给收尾。
# 之前写成 warmup+seconds+5，但脚本还要花时间等 PERF_STREAMING、锁进程，
# 结果采样跑到一半应用就退出了 —— 那轮只识别到 1 个进程就是这么来的。
app_hold=$(( warmup + seconds + 60 ))
printf 'perf:%s' "$app_hold" > "$data_dir/smoke"
echo "哨兵: $data_dir/smoke = perf:$app_hold（应用内保持推流 ${app_hold}s）"

osascript -e "tell application id \"${bundle_id}\" to quit" >/dev/null 2>&1 || true
sleep 2
echo "== 启动 =="
snapshot_pids > /tmp/apm-perf-before.txt
log_file="${HOME}/Library/Logs/AI Passport Mic/app-$(date +%Y%m%d).jsonl"
# 记录启动前的日志行数：帧率只统计本次跑产生的记录，避免混入历史。
log_baseline=1
[[ -f "$log_file" ]] && log_baseline=$(( $(wc -l <"$log_file") + 1 ))
open -a "$app"

# 等 perf 入口真正进入推流。
# **只认本次启动之后写入的 PERF_STREAMING**：`tail -40` 会看到上一轮的记录，
# 于是脚本以为已经开流、立刻开始采样，而应用其实还在启动 —— 采到的就是
# 上一轮残留 PID（或者空）。这是"只采到 4 个样本就全退出"的根因。
streaming=0
for _ in $(seq 1 40); do
  if [[ -f "$log_file" ]] && \
     tail -n +"$log_baseline" "$log_file" 2>/dev/null | grep -q 'PERF_STREAMING'; then
    streaming=1
    break
  fi
  if ! pgrep -f "apm-gui" >/dev/null 2>&1; then
    break
  fi
  sleep 1
done

if (( ! streaming )); then
  # `adapter=unavailable` 是**瞬时失败**：连续快速重启后 CoreBluetooth 还没就绪。
  # 整轮测废在这里太冤，重试一次（最多两轮）。
  if [[ -f "$log_file" ]] && tail -20 "$log_file" 2>/dev/null | grep -q 'reason=adapter=unavailable'; then
    echo "适配器探测瞬时失败（CoreBluetooth 未就绪），等 5 s 重试一次…"
    osascript -e "tell application id \"${bundle_id}\" to quit" >/dev/null 2>&1 || true
    sleep 5
    printf 'perf:%s' "$app_hold" > "$data_dir/smoke"
    snapshot_pids > /tmp/apm-perf-before.txt
    log_baseline=$(( $(wc -l <"$log_file") + 1 ))
    open -a "$app"
    for _ in $(seq 1 40); do
      if tail -n +"$log_baseline" "$log_file" 2>/dev/null | grep -q 'PERF_STREAMING'; then
        streaming=1
        break
      fi
      sleep 1
    done
  fi
fi

if (( ! streaming )); then
  echo "FAIL 没能进入推流稳态（没有看到 PERF_STREAMING）"
  echo "     最近日志："
  [[ -f "$log_file" ]] && tail -8 "$log_file" | sed 's/^/       /'
  osascript -e "tell application id \"${bundle_id}\" to quit" >/dev/null 2>&1 || true
  exit 1
fi
echo "已进入推流稳态，开始采样"

# 锁定这套进程：启动后新增的 PID 就是本 App 拉起的（含 WebKit helper）。
#
# **必须按 comm 白名单过滤**：应用启动会顺带让 macOS 按需拉起一批系统守护进程
# （实测看到 `replayd` 48 MB、`triald`、`ospredictiond`、`intelligenceflowd`、
# `osanalyticshelper`）—— 它们不是本 App 的组成部分，算进来会让内存虚高 80+ MB。
# 只收 `apm-gui` 与 WebKit 的 XPC helper。
watched=0
for attempt in 1 2 3; do
  snapshot_pids > /tmp/apm-perf-after.txt
  new_pids_since /tmp/apm-perf-before.txt /tmp/apm-perf-after.txt > /tmp/apm-perf-new.txt
  : > /tmp/apm-perf-pids.txt
  while read -r pid; do
    comm=$(ps -o comm= -p "$pid" 2>/dev/null) || continue
    case "$comm" in
      *apm-gui*|*com.apple.WebKit.*)
        echo "$pid" >> /tmp/apm-perf-pids.txt
        watched=$((watched+1))
        ;;
    esac
  done < /tmp/apm-perf-new.txt
  (( watched >= 2 )) && break
  sleep 2
done

if (( watched < 2 )); then
  echo "FAIL 只识别到 $watched 个相关进程（至少应有 apm-gui + WebKit helper）"
  echo "     差值快照可能被别的 App 干扰，或应用已提前退出。"
  echo "     当前 pid 列表：$(tr '\n' ' ' < /tmp/apm-perf-pids.txt)"
  osascript -e "tell application id \"${bundle_id}\" to quit" >/dev/null 2>&1 || true
  exit 1
fi
echo "锁定 $watched 个进程（主进程 + WebKit helper）"

# --- 采样 ------------------------------------------------------------------
# CPU：`ps %cpu` 是**自进程启动以来的平均值**，不是瞬时值，所以稳态窗口下可用，
# 但它会稀释峰值。同时用两次读取的差值算增量 CPU 更准 —— 这里用简单可靠的
# 平均 %cpu（足够分辨 3% 与 20% 这种量级差）。
samples=0
cpu_total=0
mem_max=0
max_pids=""
max_mem=""

end=$(( $(date +%s) + seconds ))
echo
echo "== 采样中（${seconds}s）=="
while (( $(date +%s) < end )); do
  cpu_sum=0
  mem_sum=0
  alive=0
  while read -r pid; do
    line=$(ps -o rss=,pcpu= -p "$pid" 2>/dev/null) || continue
    rss=$(echo "$line" | awk '{print $1}')
    pcpu=$(echo "$line" | awk '{print $2}')
    [[ -z "$rss" ]] && continue
    alive=$((alive+1))
    cpu_sum=$(awk -v a="$cpu_sum" -v b="${pcpu:-0}" 'BEGIN{printf "%.1f", a+b}')
    mem_sum=$(( mem_sum + rss ))

    # 记录每个 PID 的内存峰值（bash 3.2 没有关联数组，用并行字符串线性查找）
    found=0
    i=0
    for p in $max_pids; do
      i=$((i+1))
      if [[ "$p" == "$pid" ]]; then
        found=1
        old=$(echo "$max_mem" | cut -d' ' -f"$i")
        (( rss > old )) && max_mem=$(echo "$max_mem" | awk -v i="$i" -v v="$rss" '{ $i=v; print }')
        break
      fi
    done
    if (( ! found )); then
      max_pids="$max_pids $pid"
      max_mem="$max_mem $rss"
    fi
  done < /tmp/apm-perf-pids.txt

  if (( alive == 0 )); then
    echo "所有被监控进程都已退出，提前结束采样"
    break
  fi

  cpu_total=$(awk -v a="$cpu_total" -v b="$cpu_sum" 'BEGIN{printf "%.2f", a+b}')
  mem_mb=$(( mem_sum / 1024 ))
  (( mem_mb > mem_max )) && mem_max=$mem_mb
  samples=$(( samples + 1 ))
  sleep 1
done

if (( samples == 0 )); then
  echo "FAIL 一个样本都没采到（应用可能已退出）"
  exit 1
fi

cpu_avg=$(awk -v t="$cpu_total" -v n="$samples" 'BEGIN{printf "%.1f", t/n}')
echo "采样 $samples 次"

# --- 前端帧率 ---------------------------------------------------------------
# 由 LevelMeter 在真实渲染循环里数帧，每 5 s 上报一次（`source=fps`），
# 经 `report_fps` 命令写进诊断日志。这里读日志汇总。
fps="未测"
fps_note="日志里没有 fps 记录（可能跑的是旧包，或推流时长不足一个 5 s 窗口）"
if [[ -f "$log_file" ]]; then
  # 只取本次跑启动之后的记录：用启动时的行数做基准。
  fps_lines=$(tail -n +"$log_baseline" "$log_file" 2>/dev/null | grep -o '"message":"FPS [0-9.]*' | grep -o '[0-9.]*$' || true)
  if [[ -n "$fps_lines" ]]; then
    fps_stats=$(echo "$fps_lines" | awk '
      { n++; s+=$1; if (n==1 || $1<min) min=$1; if ($1>max) max=$1 }
      END { if (n>0) printf "%.1f %.1f %.1f %d", s/n, min, max, n }')
    fps=$(echo "$fps_stats" | awk '{print $1}')
    fps_min=$(echo "$fps_stats" | awk '{print $2}')
    fps_max=$(echo "$fps_stats" | awk '{print $3}')
    fps_n=$(echo "$fps_stats" | awk '{print $4}')
    fps_note="均值 ${fps} / 最低 ${fps_min} / 最高 ${fps_max}，共 ${fps_n} 个 5 s 窗口"
  fi
fi

echo
echo "== 结果（全部相关进程合计）=="
printf '  %-16s %s\n' "CPU 均值"  "${cpu_avg}%（口径 < ${CPU_MAX_PCT}%）"
printf '  %-16s %s\n' "内存峰值"  "${mem_max} MB（口径 < ${MEM_MAX_MB} MB）"
printf '  %-16s %s\n' "前端帧率"  "${fps}"
printf '  %-16s %s\n' "帧率明细"  "${fps_note}"

cpu_v=$(awk -v a="$cpu_avg" -v m="$CPU_MAX_PCT" 'BEGIN{exit !(a+0 < m+0)}' && echo PASS || echo FAIL)
mem_v=$(awk -v a="$mem_max" -v m="$MEM_MAX_MB" 'BEGIN{exit !(a+0 < m+0)}' && echo PASS || echo FAIL)
if [[ "$fps" == "未测" ]]; then
  fps_v="SKIP"
else
  fps_v=$(awk -v a="$fps" -v m="$FPS_MIN" 'BEGIN{exit !(a+0 >= m+0)}' && echo PASS || echo FAIL)
fi
printf '  %-16s %s\n' "CPU 判定"   "$cpu_v"
printf '  %-16s %s\n' "内存判定"  "$mem_v"
printf '  %-16s %s\n' "帧率判定"  "$fps_v"

echo
echo "== 分进程峰值明细 =="
i=0
for pid in $max_pids; do
  i=$((i+1))
  mem=$(echo "$max_mem" | cut -d' ' -f"$i")
  comm=$(ps -o comm= -p "$pid" 2>/dev/null || echo "(已退出)")
  kind="主进程"
  case "$comm" in
    *com.apple.WebKit*) kind="WebKit helper" ;;
    *) [[ "$comm" == *"AI Passport Mic.app"* ]] || kind="子进程" ;;
  esac
  printf '  %-8s %6s MB  %-14s %s\n' "$pid" "$(( mem / 1024 ))" "$kind" "$(basename "$comm")"
done

echo
if [[ "$cpu_v" == FAIL || "$mem_v" == FAIL || "$fps_v" == FAIL ]]; then
  echo "结论: FAIL"
  if [[ "$cpu_v" == FAIL || "$mem_v" == FAIL ]]; then
    echo "      占用结构见上面的明细：主进程只是小头，大头在 WebKit 的"
    echo "      WebContent（渲染电平表 canvas）与 GPU helper。"
    echo "      电平表的每帧成本已于 2026-11-07 优化（静态层/动态层拆成两张 canvas，"
    echo "      主线程 ScriptDuration -42%，见 GUI_IMPLEMENTATION_LOG.md §14.26）。"
    echo "      若这项仍超标，说明瓶颈不在每帧绘制：下一步查主进程那 86 MB，"
    echo "      以及 WebContent 里 canvas 位图之外的部分。"
  fi
  if [[ "$fps_v" == FAIL ]]; then
    echo "      前端帧率 ${fps} < ${FPS_MIN} fps —— 电平表掉帧会影响实时观感。"
  fi
  exit 1
fi

if [[ "$fps_v" == "SKIP" ]]; then
  echo "结论: CPU 与内存 PASS，但前端帧率 **未测** → G2 性能项不能整体判 PASS"
  echo "      原因：${fps_note}"
  exit 1
fi

echo "结论: PASS（CPU、内存、前端帧率三项均达口径）"
exit 0
