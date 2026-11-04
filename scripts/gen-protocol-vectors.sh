#!/usr/bin/env bash
# 生成跨端共享测试向量 config/protocol-vectors.json。
#
# 向量由**固件侧的 C 实现**导出，Rust 侧读同一份文件比对。所以：
#   改 device/main/protocol.c 之后必须重跑本脚本，否则 cargo test 会拿旧向量对拍。
#
# 用法:
#   ./scripts/gen-protocol-vectors.sh          # 写入 config/protocol-vectors.json
#   ./scripts/gen-protocol-vectors.sh --check  # 只比对当前文件是否最新（CI/校验用）
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
out="$root/config/protocol-vectors.json"
tmp_dir="${TMPDIR:-/tmp}/ai-passport-mic-vectors"
mkdir -p "$tmp_dir"
tmp_bin="$tmp_dir/gen_vectors"
tmp_json="$tmp_dir/protocol-vectors.json"

cc_bin="${CC:-cc}"
"$cc_bin" -std=c11 -O2 -Wall -Wextra -Werror \
    -I "$root/device/main" \
    "$root/device/main/protocol.c" \
    "$root/device/test/gen_vectors.c" \
    -lm -o "$tmp_bin"

"$tmp_bin" >"$tmp_json"

if [[ "${1:-}" == "--check" ]]; then
  if diff -q "$out" "$tmp_json" >/dev/null 2>&1; then
    echo "OK   config/protocol-vectors.json 与 C 实现一致"
  else
    echo "FAIL config/protocol-vectors.json 已过期（C 实现变了但没重新生成）" >&2
    echo "     跑 ./scripts/gen-protocol-vectors.sh 更新后提交" >&2
    diff -u "$out" "$tmp_json" | head -20 >&2 || true
    exit 1
  fi
else
  cp "$tmp_json" "$out"
  echo "已写入 $out"
  echo "  块数: $(grep -c '"index"' "$out" 2>/dev/null || echo 0)"
fi
