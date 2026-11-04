#!/usr/bin/env bash
# 初始化基线自检：目录结构、文档内链、脚本可执行位、环境快照。
# 当前仓库还没有代码，所以这里**不跑** cargo / idf.py / npm；
# 落地代码后再按 docs/ROADMAP.md 往对应阶段加门禁。
#
# 用法:
#   ./scripts/verify.sh           # 结构 + 内链 + 权限 + 环境快照
#   ./scripts/verify.sh --strict  # 环境不完整也算失败（CI 或交付前用）
set -uo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
strict=0
[[ "${1:-}" == "--strict" ]] && strict=1

status_structure=SKIP
status_links=SKIP
status_scripts=SKIP
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
for sub in ("docs", "config", "device", "desktop"):
    targets += sorted((root / sub).rglob("*.md"))

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
printf 'Env       : %s\n' "$status_env"
printf 'Unverified: 固件、桌面端、真机（尚未落地）\n'

if (( failed )); then
  echo
  echo "基线自检失败。"
  exit 1
fi
echo
if [[ "$status_env" == FAIL ]]; then
  echo "基线自检通过（结构/文档/脚本）；环境未就绪，固件相关命令暂不可用。"
else
  echo "初始化基线自检通过。"
fi
