# AI Passport Mic 固件工程（ESP-IDF 5.5.3 / ESP32-C3）
# 注意：IDF 要求 project() 必须在 include 之后调用。
cmake_minimum_required(VERSION 3.16)

include($ENV{IDF_PATH}/tools/cmake/project.cmake)

project(ai-passport-mic)
