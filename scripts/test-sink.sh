#!/usr/bin/env bash
# 真声卡冒烟：跑 session.rs 里被 #[ignore] 的那条测试。
#
# 为什么单独一个脚本：这条测试需要机器上装虚拟声卡（BlackHole），
# 而 CI 或者别人的机器上通常没有。以前它写成"没有声卡就 return"，
# `cargo test` 会显示 ok —— 假通过。现在默认 ignore，要用就跑这个脚本，
# 它会先探设备，没有就明说 SKIP，不假装测过。
#
# 它验的是：cpal 能按 16 kHz 打开、声卡线程能收块、clear 收尾能拿到统计。
# 它**不验**音质、延迟、长时间稳定性。
#
# 用法:
#   ./scripts/test-sink.sh
set -uo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
  echo "SKIP 没有 cargo"
  exit 0
fi

# 探设备：cpal 这边没有命令行入口，直接问系统。（测试里还会再查一次，
# 这里只是为了在没设备时给出人话提示，而不是让人看一条 expect 失败。）
if ! system_profiler SPAudioDataType 2>/dev/null | grep -qi blackhole; then
  echo "SKIP 没找到 BlackHole 之类的虚拟声卡。"
  echo "     装上之后这条测试才有意义：brew install --cask blackhole-2ch"
  exit 0
fi

echo "== 真声卡冒烟（session_with_real_sink）=="
cd "$root/desktop" || exit 1
cargo test -p apm-app session_with_real_sink -- --ignored --nocapture
status=$?

if (( status == 0 )); then
  echo
  echo "PASS 声卡打开 / 写入 / 收尾都通了（这只证明链路成立，不证明音质）"
else
  echo
  echo "FAIL 见上面的断言或 panic"
fi
exit "$status"
