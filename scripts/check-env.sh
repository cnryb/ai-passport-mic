#!/usr/bin/env bash
# 只读环境体检：工具链 + ESP-IDF v5.5.3。不修改任何文件、不写 shell 配置。
# 用法: ./scripts/check-env.sh
set -u

fail=0
check() {
  local label=$1
  shift
  if command -v "$1" >/dev/null 2>&1; then
    printf 'OK   %-14s %s\n' "$label" "$("$@" 2>/dev/null | head -1)"
  else
    printf 'MISS %-14s %s\n' "$label" "not found"
    fail=1
  fi
}

printf 'Host: %s %s\n' "$(uname -s)" "$(uname -m)"
check cc cc --version
check rustc rustc --version
check cargo cargo --version
check node node --version
check npm npm --version
check python3 python3 --version
check cmake cmake --version
check ninja ninja --version

idf_root="${IDF_PATH:-$HOME/esp/esp-idf-v5.5.3}"
if [[ ! -f "$idf_root/export.sh" ]]; then
  printf 'MISS %-14s %s\n' "ESP-IDF path" "$idf_root (export.sh 不存在)"
  echo "     按 docs/ENVIRONMENT.md 安装 ESP-IDF v5.5.3。"
  fail=1
else
  printf 'OK   %-14s %s\n' "ESP-IDF path" "$idf_root"

  # export.sh 失败最常见的原因不是版本不对，而是 python 小版本变了：它要找的
  # venv 叫 idf5.5_py3.<X>_env，<X> 是**安装当时**的 python 小版本。所以这里
  # 把 export 的真实输出留下，并单独列出磁盘上现有的 venv，别让"版本不匹配"
  # 这句笼统结论掩盖真正的原因。
  #
  # 注意：venv 缺失时 export.sh **仍然返回 0**，只是打印错误不设 PATH。所以判据
  # 不能只看退出码，必须再确认 idf.py 真的进了 PATH。
  set +u
  idf_log="${TMPDIR:-/tmp}/ai-passport-mic-idf-export.log"
  # shellcheck disable=SC1091
  source "$idf_root/export.sh" >"$idf_log" 2>&1
  if ! command -v idf.py >/dev/null 2>&1; then
    printf 'FAIL %-14s %s\n' "ESP-IDF env" "export.sh 没有把 idf.py 放进 PATH"
    echo "      当前 python3 : $(python3 --version 2>&1)"
    echo "      现有 venv    : $(ls "$HOME/.espressif/python_env" 2>/dev/null | tr '\n' ' ')"
    echo "      失败首行     : $(grep -m1 -i -e 'error' -e 'not found' "$idf_log" 2>/dev/null)"
    _venv="$(ls "$HOME/.espressif/python_env" 2>/dev/null | head -1)"
    if [[ -n "$_venv" ]]; then
      echo "      修复 A(零下载): export IDF_PYTHON_ENV_PATH=\"$HOME/.espressif/python_env/$_venv\""
    fi
    echo "      修复 B(重装)  : \"$idf_root/install.sh\" esp32c3"
    echo "      详见 docs/ENVIRONMENT.md 的「常见故障」"
    fail=1
  else
    idf_version="$(idf.py --version 2>/dev/null || true)"
    if [[ "$idf_version" == *"v5.5.3"* ]]; then
      printf 'OK   %-14s %s\n' "ESP-IDF" "$idf_version"
    else
      printf 'FAIL %-14s expected v5.5.3, got %s\n' "ESP-IDF" "${idf_version:-unknown}"
      fail=1
    fi
  fi
  rm -f "$idf_log"
  set -u
fi

if (( fail )); then
  echo
  echo "环境尚未完整；按 docs/ENVIRONMENT.md 补齐后再运行固件相关命令。"
  exit 1
fi

echo
echo "环境检查通过。下一步: source ./scripts/activate-idf.sh"
