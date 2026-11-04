#!/usr/bin/env bash
# 固件编译 + 合并镜像。需要 ESP-IDF v5.5.3。
#
# 用法:
#   ./scripts/build-firmware.sh
#
# 说明：ESP-IDF 的 python venv 与 python3 小版本绑定，系统 python 升级后默认 venv 会找不到
# （现象见 docs/ENVIRONMENT.md）。这里只在「默认 venv 不存在」时把 IDF_PYTHON_ENV_PATH
# 指到已装好的旧 venv，不联网、不装包、不改 shell 配置。
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"

if [[ -z "${IDF_PYTHON_ENV_PATH:-}" ]]; then
  _py_minor="$(python3 -c 'import sys; print(sys.version_info.minor)' 2>/dev/null || true)"
  _default="$HOME/.espressif/python_env/idf5.5_py3${_py_minor:+.$_py_minor}_env"
  if [[ ! -d "$_default" ]]; then
    _fallback="$(ls -d "$HOME"/.espressif/python_env/idf5.5_py3.*_env 2>/dev/null | head -1 || true)"
    if [[ -n "$_fallback" ]]; then
      echo "提示: 默认 venv 不存在，改用已安装的 $_fallback"
      echo "      （想长期生效见 docs/ENVIRONMENT.md 的方案 A/B）"
      export IDF_PYTHON_ENV_PATH="$_fallback"
    fi
  fi
  unset _py_minor _default _fallback
fi

# shellcheck disable=SC1091
source "$root/scripts/activate-idf.sh"

cd "$root/device"
idf.py build
# -o 相对 build/ 解析，必须给绝对路径，否则 esptool 会在 build/ 里找 build/*.bin 而失败。
idf.py merge-bin -o "$PWD/build/ai-passport-mic-merged.bin"

echo
echo "== 产物 =="
ls -l build/ai-passport-mic.bin build/ai-passport-mic-merged.bin
shasum -a 256 build/ai-passport-mic-merged.bin
echo
echo "未验证: 真机烧录、采集出声、按键标定、BLE 连接与 MTU、端到端延迟。"
