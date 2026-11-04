#!/usr/bin/env bash
# 在当前 shell 中激活 ESP-IDF v5.5.3。
# 用法: source ./scripts/activate-idf.sh

_idf_root="${IDF_PATH:-$HOME/esp/esp-idf-v5.5.3}"

if [[ ! -f "$_idf_root/export.sh" ]]; then
  echo "ESP-IDF 未找到: $_idf_root" >&2
  echo "请设置 IDF_PATH，或按 docs/ENVIRONMENT.md 安装 ESP-IDF v5.5.3。" >&2
  return 1 2>/dev/null || exit 1
fi

# export.sh 会输出激活提示；正常输出不刷屏，但必须留一份日志用于失败归因。
# 注意：venv 缺失时 export.sh **仍然返回 0**，只打印错误并不设 PATH，
# 所以判据是"idf.py 到底可不可用"，不是它的退出码。
_idf_log="${TMPDIR:-/tmp}/ai-passport-mic-idf-activate.log"
# shellcheck disable=SC1091
source "$_idf_root/export.sh" >"$_idf_log" 2>&1

if ! command -v idf.py >/dev/null 2>&1; then
  echo "ESP-IDF 激活失败：export.sh 没有把 idf.py 放进 PATH。" >&2
  echo "  当前 python3 : $(python3 --version 2>&1)" >&2
  echo "  现有 venv    : $(ls "$HOME/.espressif/python_env" 2>/dev/null | tr '\n' ' ')" >&2
  echo "  失败首行     : $(grep -m1 -i -e 'error' -e 'not found' "$_idf_log" 2>/dev/null)" >&2
  _venv="$(ls "$HOME/.espressif/python_env" 2>/dev/null | head -1)"
  if [[ -n "$_venv" ]]; then
    echo "  修复 A(零下载): export IDF_PYTHON_ENV_PATH=\"$HOME/.espressif/python_env/$_venv\"" >&2
  fi
  echo "  修复 B(重装)  : \"$_idf_root/install.sh\" esp32c3" >&2
  echo "  详见 docs/ENVIRONMENT.md 的「常见故障」" >&2
  rm -f "$_idf_log"
  unset _idf_root
  return 1 2>/dev/null || exit 1
fi
rm -f "$_idf_log"

_idf_version="$(idf.py --version 2>/dev/null || true)"
if [[ "$_idf_version" != *"v5.5.3"* ]]; then
  echo "ESP-IDF 版本不匹配: expected v5.5.3, got ${_idf_version:-unknown}" >&2
  unset _idf_root _idf_version
  return 1 2>/dev/null || exit 1
fi

echo "ESP-IDF 已激活: $_idf_version"
unset _idf_root _idf_version
