#!/usr/bin/env bash
# 固件主机侧单测：用系统 clang 直接编译，不依赖 ESP-IDF、不依赖硬件。
# 这是目前唯一能真正"跑起来"的固件相关检查 —— 编译通过不等于逻辑正确。
#
# 两个测试目标，都只编译**不依赖 ESP-IDF 的纯逻辑模块**：
#   protocol.c       CRC / 帧 / ADPCM / 毛刺判据 / PING-PONG 长度校验
#   power_policy.c   连接参数两档 / LINK_SLOW 判据 / 采集态状态机 / 发射功率滞回
#
# 用法:
#   ./scripts/test-protocol.sh
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
out_dir="${TMPDIR:-/tmp}/ai-passport-mic-protocol-test"
mkdir -p "$out_dir"

cc_bin="${CC:-cc}"
cflags=(-std=c11 -O2 -Wall -Wextra -Werror -I "$root/device/main")

echo "== protocol =="
"$cc_bin" "${cflags[@]}" \
    "$root/device/main/protocol.c" \
    "$root/device/test/test_protocol.c" \
    -lm -o "$out_dir/test_protocol"
"$out_dir/test_protocol"

echo
echo "== power_policy =="
# 省电逻辑（2026-11-07 落地时的缺口就在这里）：两档连接参数、LINK_SLOW 只在采集态判、
# 采集态状态机、发射功率滞回。这些原先埋在 NimBLE 回调与 FreeRTOS 任务里，
# 主机侧根本编不起来，等于"改了省电逻辑但没有任何测试能跑"。
"$cc_bin" "${cflags[@]}" \
    "$root/device/main/power_policy.c" \
    "$root/device/test/test_power_policy.c" \
    -o "$out_dir/test_power_policy"
"$out_dir/test_power_policy"
