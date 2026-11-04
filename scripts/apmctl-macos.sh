#!/usr/bin/env bash
# 以 app bundle 身份运行 apmctl。
#
# 为什么需要这层包装：
#   macOS 的 TCC 只认「带用途声明的 app bundle + LaunchServices 启动」。
#   直接跑 desktop/target/release/apmctl 会被系统以「缺少 NSBluetoothAlwaysUsageDescription」
#   直接 abort（崩溃报告里 namespace=TCC）；WorkBuddy / Terminal / Warp 都没有这个键，
#   所以从它们启动的子进程一律不行。
#   而把 apmctl 本身做成 app 也有坑：每次换二进制就要重新签名，LaunchServices 会认不出来，
#   直接用 "Unable to block on application" 拒绝启动。
#
# 结构（两层，各自职责明确）：
#   APMRun.app   稳定的启动器（编译一次就固定，签名不随 apmctl 变）
#     └─ 子进程 APMCTL.app/Contents/MacOS/apmctl（随时可替换）
#   输出由 apmctl 的 --log 自己写文件，因为经 LaunchServices 启动拿不到 stdout。
#
# 用法：
#   ./scripts/apmctl-macos.sh scan --timeout 6
#   ./scripts/apmctl-macos.sh sink --to "BlackHole 2ch" --seconds 30
#
# 首次运行会弹一次「想要使用蓝牙」，点允许即可（按 bundle 记住）。
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bin="$root/desktop/target/release/apmctl"

if [[ ! -x "$bin" ]]; then
  echo "找不到 $bin，先构建：cd desktop && cargo build --release" >&2
  exit 1
fi

base="${APMCTL_HOME:-/tmp/ai-passport-mic}"
child="$base/APMCTL.app"
runner="$base/APMRun.app"

# --- 子进程 app：真正干活的二进制 -------------------------------------------
mkdir -p "$child/Contents/MacOS"
cp "$bin" "$child/Contents/MacOS/apmctl"
cat > "$child/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleExecutable</key><string>apmctl</string>
  <key>CFBundleIdentifier</key><string>com.ai-passport-mic.apmctl</string>
  <key>CFBundleName</key><string>apmctl</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>LSUIElement</key><true/>
  <key>NSBluetoothAlwaysUsageDescription</key><string>apmctl 需要蓝牙 LE 连接 AI Passport Mic 设备，接收音频块与状态。</string>
  <key>NSBluetoothPeripheralUsageDescription</key><string>apmctl 需要蓝牙 LE 连接 AI Passport Mic 设备。</string>
  <key>NSMicrophoneUsageDescription</key><string>回环验证需要从虚拟声卡输入端读取音频。</string>
</dict></plist>
PLIST
codesign --force --sign - --identifier com.ai-passport-mic.apmctl "$child" >/dev/null 2>&1

# --- 启动器 app：编译一次即可，apmctl 怎么换都不影响它的身份 ----------------
mkdir -p "$runner/Contents/MacOS"
if [[ ! -x "$runner/Contents/MacOS/apmrun" || "$root/scripts/apm-run.c" -nt "$runner/Contents/MacOS/apmrun" ]]; then
  cc -O2 -o "$runner/Contents/MacOS/apmrun" "$root/scripts/apm-run.c"
fi
cat > "$runner/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleExecutable</key><string>apmrun</string>
  <key>CFBundleIdentifier</key><string>com.ai-passport-mic.apmrun</string>
  <key>CFBundleName</key><string>apmrun</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSUIElement</key><true/>
  <key>NSBluetoothAlwaysUsageDescription</key><string>apmrun 需要蓝牙 LE 连接 AI Passport Mic 设备接收音频。</string>
  <key>NSMicrophoneUsageDescription</key><string>回环验证需要从虚拟声卡输入端读取音频。</string>
</dict></plist>
PLIST
codesign --force --sign - --identifier com.ai-passport-mic.apmrun "$runner" >/dev/null 2>&1

# 参数走固定作业文件：经 LaunchServices 启动时 --args 不保证转发（实测为空）
log="$(mktemp -t apmctl)"
job="$base/job"
{
  echo "$child/Contents/MacOS/apmctl"
  echo "$log"
  for a in "$@"; do echo "$a"; done
} > "$job"

open -W -a "$runner"
cat "$log"
rm -f "$log" "$job"
