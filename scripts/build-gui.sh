#!/usr/bin/env bash
# 构建 macOS 的 .app，并验证 TCC 用途声明真的进了 Info.plist。
#
# 为什么必须查 Info.plist：缺 NSBluetoothAlwaysUsageDescription 时应用不是"没权限"，
# 而是调用 CoreBluetooth 直接被系统 abort。构建产物看起来正常，一开就崩。
#
# 用法:
#   ./scripts/build-gui.sh            # debug 构建（默认，够验收）
#   ./scripts/build-gui.sh --release  # release 构建
set -uo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
gui="$root/desktop/crates/gui"
front="$gui/frontend"
profile="${1:---debug}"

if [[ ! -x "$front/node_modules/.bin/tauri" ]]; then
  echo "== 前端依赖 =="
  ( cd "$front" && npm install --no-audit --no-fund ) || { echo "npm install 失败"; exit 1; }
fi

echo "== 前端构建 =="
( cd "$front" && npm run build ) || { echo "前端构建失败"; exit 1; }

echo
# GUI 要能烧固件，安装包里就必须带一份镜像。缺了直接停，
# 否则会打出一个"装得上、点烧写才报错"的包——那种包发给客户最伤。
echo "== 随包固件 =="
fw_src="$root/device/build/ai-passport-mic-merged.bin"
fw_dst="$gui/resources/ai-passport-mic-merged.bin"
if [[ ! -f "$fw_src" ]]; then
  echo "FAIL 缺少 $fw_src"
  echo "     先跑 ./scripts/build-firmware.sh（烧写功能依赖这份合并镜像）"
  exit 1
fi
mkdir -p "$gui/resources"
cp "$fw_src" "$fw_dst"
echo "OK   $(basename "$fw_src") → crates/gui/resources（$(wc -c <"$fw_dst" | tr -d ' ') 字节）"

echo
# 变量后面紧跟中文全角括号时，某些 locale 下 bash 会把括号字节当成变量名的一部分，
# 报 "profile: unbound variable"。一律用 ${} 包起来。
echo "== Tauri 打包: ${profile} =="
# Tauri 2 里 release 是默认行为，没有 --release 这个 flag（传了会被当成位置参数报错）；
# 只有 --debug 可选。所以这里按 profile 决定要不要加 -d，不能把 profile 原样传进去。
if [[ "$profile" == "--release" ]]; then
  tauri_args=(--bundles app)
else
  tauri_args=(--debug --bundles app)
fi
( cd "$gui" && ./frontend/node_modules/.bin/tauri build "${tauri_args[@]}" ) || {
  echo "打包失败，看上面的 cargo 输出"
  exit 1
}

app="$root/desktop/target/debug/bundle/macos/AI Passport Mic.app"
[[ "$profile" == "--release" ]] && app="$root/desktop/target/release/bundle/macos/AI Passport Mic.app"
plist="$app/Contents/Info.plist"

echo
echo "== 产物与权限声明 =="
failed=0
if [[ -d "$app" ]]; then
  echo "OK   $app"
else
  echo "MISS 找不到 $app（Tauri 的 target 目录可能与预期不同）"
  failed=1
fi

if [[ -f "$plist" ]]; then
  for key in NSBluetoothAlwaysUsageDescription NSMicrophoneUsageDescription CFBundleIdentifier; do
    if value=$(/usr/libexec/PlistBuddy -c "Print :$key" "$plist" 2>/dev/null); then
      echo "OK   $key = $value"
    else
      echo "FAIL Info.plist 缺少 $key"
      failed=1
    fi
  done
  bundle_id=$(/usr/libexec/PlistBuddy -c "Print :CFBundleIdentifier" "$plist" 2>/dev/null || true)
  if [[ "$bundle_id" != "com.cnryb.ai-passport-mic" ]]; then
    # bundle id 变了会让 macOS 把已有 TCC 授权全部作废，必须挡住。
    echo "FAIL bundle identifier 必须固定为 com.cnryb.ai-passport-mic（当前: ${bundle_id:-空}）"
    failed=1
  fi
else
  echo "MISS 没有 Info.plist"
  failed=1
fi

# 随包固件：资源没进包，烧写页会显示"安装包里的固件缺失"。
# 这是打包问题，不是运行时问题，必须在构建阶段就拦住。
fw_in_app="$app/Contents/Resources/firmware/ai-passport-mic-merged.bin"
if [[ -f "$fw_in_app" ]]; then
  echo "OK   随包固件 $(wc -c <"$fw_in_app" | tr -d ' ') 字节"
else
  echo "FAIL .app 里没有 firmware/ai-passport-mic-merged.bin（检查 tauri.conf.json 的 bundle.resources）"
  failed=1
fi

echo
if (( failed )); then
  echo "GUI 构建未通过。"
  exit 1
fi
echo "GUI 构建通过。启动方式：open \"$app\"（不要直接跑 target 里的裸二进制）"
