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
#
# 这里**只校验，不替用户跑 build-firmware.sh**：打包刻意不依赖 ESP-IDF
# （CI 与别人的机器上通常没装）。所以过期就拦下来，让人自己决定要不要重编。
echo "== 随包固件 =="
fw_src="$root/device/build/ai-passport-mic-merged.bin"
fw_dst="$gui/resources/ai-passport-mic-merged.bin"
if [[ ! -f "$fw_src" ]]; then
  echo "FAIL 缺少 $fw_src"
  echo "     先跑 ./scripts/build-firmware.sh（烧写功能依赖这份合并镜像）"
  exit 1
fi

# 镜像比固件源码旧 = 改了固件但没重编。这时下面那条 cp 会把一份**陈旧镜像**
# 堂堂正正打进包，烧写页照样"写入成功 + MD5 一致"，人根本看不出来。
# 2026-11-08 就是这么发现问题的：device/build 是 11-08、包里还是 11-07 的产物。
# 排除 build/（产物）、managed_components/（第三方）、test/（主机侧单测，不进固件）。
fw_stale="$(find "$root/device" \
  \( -path "$root/device/build" -o -path "$root/device/managed_components" \
     -o -path "$root/device/test" \) -prune -o \
  -type f \( -name '*.c' -o -name '*.h' -o -name 'CMakeLists.txt' \
             -o -name '*.csv' -o -name 'sdkconfig.defaults' -o -name 'idf_component.yml' \) \
  -newer "$fw_src" -print 2>/dev/null)"
if [[ -n "$fw_stale" ]]; then
  echo "FAIL 固件源码比 $fw_src 新，说明改完固件没重编"
  echo "     先跑 ./scripts/build-firmware.sh，否则打进包的是旧镜像："
  printf '     %s\n' $fw_stale | head -8
  exit 1
fi
echo "OK   镜像不比 device/ 下的源码旧"

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
#
# 只断言"文件存在且非空"是不够的：上一版就是这么写的，结果 2026-11-08 的包里
# 装着 11-07 的镜像（resources/ 那份是旧的，cp 前没人比过），照样 PASS。
# 所以这里比对**与源文件的字节数 + 内容哈希**，包里的必须就是刚复制进去的那份。
fw_in_app="$app/Contents/Resources/firmware/ai-passport-mic-merged.bin"
if [[ ! -f "$fw_in_app" ]]; then
  echo "FAIL .app 里没有 firmware/ai-passport-mic-merged.bin（检查 tauri.conf.json 的 bundle.resources）"
  failed=1
else
  src_size=$(wc -c <"$fw_src" | tr -d ' ')
  app_size=$(wc -c <"$fw_in_app" | tr -d ' ')
  src_hash=$(shasum -a 256 "$fw_src" | awk '{print $1}')
  app_hash=$(shasum -a 256 "$fw_in_app" | awk '{print $1}')
  if [[ "$src_size" != "$app_size" ]]; then
    echo "FAIL 包内固件 ${app_size} 字节 ≠ 源镜像 ${src_size} 字节"
    failed=1
  elif [[ "$src_hash" != "$app_hash" ]]; then
    echo "FAIL 包内固件与 source 镜像的 sha256 不一致（字节数都是 ${src_size}，内容不同）"
    failed=1
  else
    echo "OK   随包固件 ${app_size} 字节，sha256 与源镜像一致（${src_hash:0:12}…）"
  fi
fi

echo
if (( failed )); then
  echo "GUI 构建未通过。"
  exit 1
fi
echo "GUI 构建通过。启动方式：open \"$app\"（不要直接跑 target 里的裸二进制）"
