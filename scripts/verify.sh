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
status_font=SKIP
status_vectors=SKIP
status_desktop=SKIP
status_publish=SKIP
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
echo "== 固件主机侧单测 =="
# 这是固件侧唯一能真正跑起来的检查：编译通过不等于 CRC / 帧 / ADPCM / 省电逻辑 /
# 屏幕渲染正确。覆盖三个纯逻辑模块：
#   protocol      协议（CRC / 帧 / ADPCM / 毛刺 / PING-PONG / SET_KEY_LABELS）
#   power_policy  两档连接参数、LINK_SLOW 判据、采集态状态机、发射功率滞回
#   screen        渲染（UTF-8 / 缺字形 / 越界 / 逐带等价）与亮灭状态机
proto_log="${TMPDIR:-/tmp}/ai-passport-mic-verify-proto.log"
if "$root/scripts/test-protocol.sh" >"$proto_log" 2>&1; then
  status_proto=PASS
  # 三个目标各一行 "N passed, M failed"，加起来报总数，别只报最后一行的数。
  total=$(awk '/^[0-9]+ passed, / {s+=$1} END {print s+0}' "$proto_log")
  echo "OK   固件纯逻辑单测（合计 $total passed）"
else
  status_proto=FAIL
  failed=1
  echo "FAIL 固件主机侧单测未通过，明细:"
  sed 's/^/      /' "$proto_log"
fi
rm -f "$proto_log"

echo
echo "== 屏幕点阵字库 =="
# 字库必须与 screen_texts.h 的文案一一对应。加了新文案却忘了重新生成字库，
# 屏幕上就是一个方框 —— 那种缺陷只有上真机才看得见，所以在这里拦。
status_font=SKIP
font_log="${TMPDIR:-/tmp}/ai-passport-mic-verify-font.log"
if python3 "$root/scripts/gen-font-subset.py" --check >"$font_log" 2>&1; then
  status_font=PASS
  sed 's/^OK   /     /' "$font_log"
else
  status_font=FAIL
  failed=1
  echo "FAIL 字库与文案不一致:"
  sed 's/^/      /' "$font_log" | head -10
fi
rm -f "$font_log"

echo
echo "== 跨端向量 =="# 向量是固件 C 实现导出的。C 改了没重生成，Rust 对拍就会拿旧数据当"通过"。
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
    # 只数"跑过测试"的目标：workspace 里有几个目标本身 0 测试（纯二进制入口），
    # 把它们算进"测试目标"会让计数看起来比实际多，是文档数字反复过时的来源之一。
    passed=$(awk '/^test result: ok\./ {s+=$4} END {print s+0}' "$test_log")
    targets=$(awk '/^test result: ok\./ && $4+0 > 0 {n++} END {print n+0}' "$test_log")
    ignored=$(awk '/^test result: ok\./ {s+=$8} END {print s+0}' "$test_log")
    echo "OK   cargo test 通过（$targets 个有测试的目标，$passed passed / $ignored ignored）"
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
echo "== 发布文案 =="
# 平台硬规则：play-first 文案里不能出现芯片/外设/SDK 这类工程词，
# 封面必须是严格 3:4。这些以前只能靠人眼看，而"改了界面忘了改文案"
# 是这类交付最常见的漏项 —— 文案里写着「检测设备」，界面上叫「检测版本」，
# 用户照着点不到。所以这里把能自动查的都查掉。
pub_dir="$root/docs/publish"
pub_problems=0
if [[ ! -d "$pub_dir" ]]; then
  echo "SKIP 没有 docs/publish（还没准备发布物）"
else
  # 1) 必需文件
  for f in description-zh.txt description-en.txt instructions-zh.txt instructions-en.txt cover.jpg; do
    if [[ ! -f "$pub_dir/$f" ]]; then
      echo "FAIL 缺 docs/publish/$f"
      pub_problems=1
    fi
  done

  # 2) 字符数上限（中文简介 4000 / 使用方法 8000 / 更新日志 4000；英文同理放宽后同阈值）
  if [[ -f "$pub_dir/description-zh.txt" ]]; then
    n=$(wc -m <"$pub_dir/description-zh.txt" | tr -d ' ')
    (( n < 10 || n > 4000 )) && { echo "FAIL description-zh 长度 $n，要求 10–4000"; pub_problems=1; }
  fi
  for f in instructions-zh.txt instructions-en.txt; do
    [[ -f "$pub_dir/$f" ]] || continue
    n=$(wc -m <"$pub_dir/$f" | tr -d ' ')
    (( n == 0 || n > 8000 )) && { echo "FAIL $f 长度 $n，要求 1–8000"; pub_problems=1; }
  done

  # 3) 禁用词：芯片、外设、总线、SDK 一类。命中即 FAIL ——
  #    这条是平台明文规则（见 docs/PUBLISHING.md §1），不是我们自己的偏好。
  #    注意 `flashing` / `flashes` 不是 `flash`，用词边界避免误报。
  banned='ESP32|Espressif|乐鑫|ES8311|I2C|I2S|SPI|UART|GPIO|LVGL|OLED|LCD|NimBLE|ADPCM|CJK|sdkconfig|ESP-IDF'
  if rg -q -i "\b($banned)\b" "$pub_dir"/*.txt 2>/dev/null; then
    echo "FAIL 发布文案里出现工程词（平台要求 play-first）:"
    rg -n -i "\b($banned)\b" "$pub_dir"/*.txt | head -5 | sed 's/^/      /'
    pub_problems=1
  fi

  # 4) 纯文本字段里不该有 Markdown（`**粗体**` 会原样显示成星号）
  if rg -q '\*\*|^#{1,6} ' "$pub_dir"/*.txt 2>/dev/null; then
    echo "FAIL 发布文案里有 Markdown 标记（这些字段是纯文本）"
    rg -n '\*\*|^#{1,6} ' "$pub_dir"/*.txt | head -5 | sed 's/^/      /'
    pub_problems=1
  fi

  # 5) 封面严格 3:4
  if [[ -f "$pub_dir/cover.jpg" ]]; then    dims=$(sips -g pixelWidth -g pixelHeight "$pub_dir/cover.jpg" 2>/dev/null \
      | awk '/pixelWidth/{w=$2} /pixelHeight/{h=$2} END{print w" "h}')
    read -r cw ch <<<"$dims"
    if [[ -z "${cw:-}" || -z "${ch:-}" ]]; then
      echo "FAIL 读不到封面尺寸（sips 失败）"
      pub_problems=1
    elif (( cw * 4 != ch * 3 )); then
      echo "FAIL 封面必须是严格 3:4 竖版，当前 ${cw}×${ch}"
      pub_problems=1
    fi
  fi

  # 6) Release 说明：ad-hoc 分发时，这是用户唯一能看到"首次打开被拦怎么办"的地方。
  #    文案里必须写出去隔离属性的办法，否则用户下载完就卡住了。
  if [[ ! -s "$pub_dir/release-notes.md" ]]; then
    echo "FAIL 缺 docs/publish/release-notes.md（Release 页的用户说明）"
    pub_problems=1
  elif ! rg -q 'xattr -dr com\.apple\.quarantine' "$pub_dir/release-notes.md"; then
    echo "FAIL release-notes.md 里没有 xattr 去隔离指引"
    echo "     当前是 ad-hoc 签名，用户首次打开一定会被 Gatekeeper 拦。"
    pub_problems=1
  fi

  if (( pub_problems == 0 )); then
    echo "OK   文案与封面满足平台硬规则（长度 / 禁用词 / 纯文本 / 3:4）"
  fi
fi
(( pub_problems )) && { status_publish=FAIL; failed=1; } || status_publish=PASS

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
printf 'Font      : %s\n' "$status_font"
printf 'Vectors   : %s\n' "$status_vectors"
printf 'Desktop   : %s\n' "$status_desktop"
printf 'Publish   : %s\n' "$status_publish"
printf 'Env       : %s\n' "$status_env"
printf 'Firmware  : SKIP（idf.py build 不在本脚本内，改固件后跑 ./scripts/build-firmware.sh）\n'
printf 'GUI build : SKIP（tauri 依赖重，单独跑 ./scripts/build-gui.sh）\n'
printf 'Unverified: 设备屏幕的**观感与手势**（初始化/自动熄屏/标签下发已于 2026-11-08 真机 PASS；未验：面板画面对不对（反色/镜像/观感）、长按切屏与灭屏唤醒的手感、ST7789 是否真进 Sleep In、背光电流收益——都要人眼看+手按+量电流）、关麦兜底只验了 1 min 触发（机制正确），10 min 默认口径未跑、UI 上改时长未点过、KI-005 两条护栏只验了"不误伤正常会话"（"拦得住脏会话"仍只到单测，本机造不出慢档）、按键电压窗口未用万用表复核（分类结果已验）、端到端声学延迟、G2 的 CPU/内存两项（2026-11-07 release 包实测 CPU 14.2%%、内存 208 MB，均超口径；前端 59.8 fps 已 PASS；欠载口径已改为阈值型，600 s 实测 1.0 次/分 → PASS；用 ./scripts/test-gui-perf.sh 复现）、GUI 真机验收（G4 场景矩阵）、M6 空片首烧与中断恢复；GUI 扫描的自动路径见 docs/GUI_IMPLEMENTATION_LOG.md §14.5\n'
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
    echo "固件编译请跑 ./scripts/build-firmware.sh；GUI 构建请跑 ./scripts/build-gui.sh；虚拟声卡输出与链路 RTT 已真机验证；三键 PRESS、按键注入端到端、15 项手势码已于 2026-11-06 真机通过（docs/GUI_IMPLEMENTATION_LOG.md §14.16/§14.17），端到端声学延迟与按键电压窗口边界未验证。"
fi
