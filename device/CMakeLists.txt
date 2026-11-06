# AI Passport Mic 固件工程（ESP-IDF 5.5.3 / ESP32-C3）
# 注意：IDF 要求 project() 必须在 include 之后调用。
cmake_minimum_required(VERSION 3.16)

include($ENV{IDF_PATH}/tools/cmake/project.cmake)

# 固件版本：固定 semver，供 PC 端做版本比对
# （见 docs/ARCHITECTURE.md 的「固件烧写与版本比对」）。
#
# 不设置它时，ESP-IDF 会用 `git describe --always --tags --dirty` 填
# esp_app_desc_t.version，产出形如 `7552506-dirty` 的串——带 git hash，且 `-dirty`
# 说明构建时工作区有未提交改动。这种值既不能给客户看，也没法做版本序比较。
#
# 改这里要同步：docs/ROADMAP.md 的版本记录 + 重跑 ./scripts/build-firmware.sh。
set(PROJECT_VER "0.1.0")

project(ai-passport-mic)
