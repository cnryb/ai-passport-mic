#!/usr/bin/env bash
# 基线自检：目录结构、文档内链、脚本可执行位、协议单测、环境快照。
#
# 协议单测（scripts/test-protocol.sh）在这里跑，因为它是固件侧唯一不依赖硬件的
# 正确性检查。idf.py build **不在**这里：它要完整 IDF 环境、两分钟起步，
# 改固件后单独跑 ./scripts/build-firmware.sh 并如实报告结果。
#
# 用法:
#   ./scripts/verify.sh           # 结构 + 内链 + 权限 + 协议单测 + 环境快照
#   ./scripts/verify.sh --strict  # 环境不完整也算失败（CI 或交付前用）
set -uo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
strict=0
[[ "${1:-}" == "--strict" ]] && strict=1

status_structure=SKIP
status_links=SKIP
status_scripts=SKIP
status_proto=SKIP
status_vectors=SKIP
status_desktop=SKIP
status_env=SKIP
failed=0

echo "== 目录结构 =="
missing=()
for p in README.md AGENTS.md LICENSE .gitignore \
         config/button-map.example.json config/README.md \
         device/README.md desktop/README.md \
         scripts/check-env.sh scripts/activate-idf.sh scripts/verify.sh \
         docs/PRODUCT.md docs/ARCHITECTURE.md docs/ENVIRONMENT.md \
         docs/SOUND_QUALITY.md docs/ROADMAP.md docs/REFERENCES.md; do
  [[ -e "$root/$p" ]] || missing+=("$p")
done
if (( ${#missing[@]} )); then
  printf 'MISS %s\n' "${missing[@]}"
  status_structure=FAIL
  failed=1
else
  echo "OK   必需文件齐全"
  status_structure=PASS
fi

echo
echo "== 文档内链 =="
if command -v python3 >/dev/null 2>&1; then
  if python3 - "$root" <<'PY'
import re, sys
from pathlib import Path

root = Path(sys.argv[1])
link = re.compile(r'!?\[[^\]]*\]\(([^)\s]+)(?:\s+"[^"]*")?\)')
targets = [root / "README.md", root / "AGENTS.md"]
# 只查本仓库自己的文档：node_modules / target / dist 里的第三方 README 不是我们维护的，
# 它们的相对链接在包内本来就可能指不到（例：nanoid 的 README.ru.md），扫进来只会制造噪音。
skip_dirs = {"node_modules", "target", "dist", ".cargo", "vendor"}
for sub in ("docs", "config", "device", "desktop"):
    for path in sorted((root / sub).rglob("*.md")):
        if skip_dirs & set(path.parts):
            continue
        targets.append(path)

bad = []
for f in targets:
    if not f.is_file():
        continue
    for m in link.finditer(f.read_text(encoding="utf-8")):
        t = m.group(1)
        if t.startswith(("http://", "https://", "mailto:", "#")):
            continue
        t = t.split("#")[0]
        if not t:
            continue
        if not (f.parent / t).resolve().exists():
            bad.append(f"{f.relative_to(root)} -> {t}")

for line in bad:
    print("MISS " + line)
sys.exit(1 if bad else 0)
PY
  then
    echo "OK   所有相对链接可解析"
    status_links=PASS
  else
    status_links=FAIL
    failed=1
  fi
else
  echo "SKIP 没有 python3，跳过内链检查"
fi

echo
echo "== 脚本可执行位 =="
# 必须用 $root 拼路径：脚本可能被从任意目录调用，相对 glob 会退化成字面量
# "scripts/*.sh" 从而误报缺 +x。
shopt -s nullglob
not_exec=()
for s in "$root"/scripts/*.sh; do
  [[ -x "$s" ]] || not_exec+=("${s#"$root"/}")
done
if (( ${#not_exec[@]} )); then
  printf 'MISS 缺少 +x: %s\n' "${not_exec[*]}"
  status_scripts=FAIL
  failed=1
else
  echo "OK   scripts/*.sh 均可执行"
  status_scripts=PASS
fi

echo
echo "== 协议单测 =="
# 这是固件侧唯一能真正跑起来的检查：编译通过不等于 CRC / 帧 / ADPCM 正确。
proto_log="${TMPDIR:-/tmp}/ai-passport-mic-verify-proto.log"
if "$root/scripts/test-protocol.sh" >"$proto_log" 2>&1; then
  status_proto=PASS
  echo "OK   $(tail -1 "$proto_log")"
else
  status_proto=FAIL
  failed=1
  echo "FAIL 协议单测未通过，明细:"
  sed 's/^/      /' "$proto_log"
fi
rm -f "$proto_log"

echo
echo "== 跨端向量 =="
# 向量是固件 C 实现导出的。C 改了没重生成，Rust 对拍就会拿旧数据当"通过"。
vec_log="${TMPDIR:-/tmp}/ai-passport-mic-verify-vectors.log"
if "$root/scripts/gen-protocol-vectors.sh" --check >"$vec_log" 2>&1; then
  status_vectors=PASS
  echo "OK   config/protocol-vectors.json 与 C 实现一致"
else
  status_vectors=FAIL
  failed=1
  echo "FAIL 向量已过期或生成失败:"
  sed 's/^/      /' "$vec_log" | head -15
fi
rm -f "$vec_log"

echo
echo "== 桌面端测试 =="
if [[ -f "$root/desktop/Cargo.toml" ]] && command -v cargo >/dev/null 2>&1; then
  test_log="${TMPDIR:-/tmp}/ai-passport-mic-verify-cargo.log"
  if ( cd "$root/desktop" && cargo test --quiet ) >"$test_log" 2>&1; then
    status_desktop=PASS
    echo "OK   cargo test 通过（$(grep -c 'test result: ok' "$test_log" 2>/dev/null || echo 1) 个测试目标）"
  else
    status_desktop=FAIL
    failed=1
    echo "FAIL cargo test 未通过:"
    grep -E 'test result: FAILED|error\[|panicked' "$test_log" | head -10 | sed 's/^/      /'
  fi
  rm -f "$test_log"
else
  status_desktop=SKIP
  echo "SKIP 没有 cargo 或 desktop/Cargo.toml"
fi

echo
echo "== 环境快照（不影响基线结论）=="
env_log="${TMPDIR:-/tmp}/ai-passport-mic-verify-env.log"
if "$root/scripts/check-env.sh" >"$env_log" 2>&1; then
  status_env=PASS
  echo "OK   工具链与 ESP-IDF v5.5.3 就绪"
else
  status_env=FAIL
  echo "WARN 环境未就绪，明细如下（固件相关命令暂时不可用）:"
  sed 's/^/      /' "$env_log"
  (( strict )) && failed=1
fi
rm -f "$env_log"

echo
echo "== 结果 =="
printf 'Structure : %s\n' "$status_structure"
printf 'Docs links: %s\n' "$status_links"
printf 'Scripts   : %s\n' "$status_scripts"
printf 'Protocol  : %s\n' "$status_proto"
printf 'Vectors   : %s\n' "$status_vectors"
printf 'Desktop   : %s\n' "$status_desktop"
printf 'Env       : %s\n' "$status_env"
printf 'Firmware  : SKIP（idf.py build 不在本脚本内，改固件后跑 ./scripts/build-firmware.sh）\n'
printf 'GUI build : SKIP（tauri 依赖重，单独跑 ./scripts/build-gui.sh）\n'
printf 'Unverified: 按键手势逐项核对、端到端声学延迟、G2 的"0 欠载"（10 min 实测 10 次欠载，KI-006）、GUI 真机验收（G1 断连、G3 对拍）；GUI 扫描的自动路径见 GUI.md §14.5\n'
printf 'Known issues: docs/KNOWN_ISSUES.md（KI-001 毛刺根因未定位，不要写成已解决）\n'

if (( failed )); then
  echo
  echo "基线自检失败。"
  exit 1
fi
echo
if [[ "$status_env" == FAIL ]]; then
  echo "基线自检通过（结构/文档/脚本/协议/向量/桌面端）；环境未就绪，固件编译暂不可用。"
else
  echo "基线自检通过（结构/文档/脚本/协议/向量/桌面端/环境）。"
  echo "固件编译请跑 ./scripts/build-firmware.sh；GUI 构建请跑 ./scripts/build-gui.sh；虚拟声卡输出与链路 RTT 已真机验证，按键手势与端到端声学延迟未验证。"
fi
