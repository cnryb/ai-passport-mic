#!/usr/bin/env bash
# 协议层主机侧单测：用系统 clang 编译 device/main/protocol.c，不依赖 ESP-IDF。
# 这是目前唯一能真正"跑起来"的固件相关检查 —— 编译通过不等于协议正确，
# 所以 CRC / 帧 / ADPCM 在这里用已知向量和往返指标验证。
#
# 用法:
#   ./scripts/test-protocol.sh
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
out_dir="${TMPDIR:-/tmp}/ai-passport-mic-protocol-test"
mkdir -p "$out_dir"

cc_bin="${CC:-cc}"

"$cc_bin" -std=c11 -O2 -Wall -Wextra -Werror \
    -I "$root/device/main" \
    "$root/device/main/protocol.c" \
    "$root/device/test/test_protocol.c" \
    -lm -o "$out_dir/test_protocol"

"$out_dir/test_protocol"
