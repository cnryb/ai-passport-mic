# 开发环境

## 目标与版本

| 项 | 值 | 说明 |
| --- | --- | --- |
| 目标芯片 | ESP32-C3 | 单核 RISC-V，仅 BLE（无经典蓝牙） |
| Flash | 8 MB | 分区表按 8 MB 设计 |
| PSRAM | 无 | 不要开启 SPIRAM 相关配置 |
| 固件工具链 | **ESP-IDF v5.5.3** | 固定版本，不要用其他版本"试试看" |
| PC 端 Rust | stable（落地时在 `desktop/rust-toolchain.toml` 钉住） | 当前宿主为 1.95.0 |
| 前端 | Node.js 22 LTS 及以上 | 当前宿主为 v22.22.2 |
| Python | 跟随 ESP-IDF 安装脚本 | 当前宿主为 3.13.12 |
| 宿主机 | macOS 15.3.1 arm64（已实测） | Windows 路径尚未验证 |

## 实机硬件（与固件契约相关）

| 项 | 事实 | 与固件的关系 |
| --- | --- | --- |
| **电源键** | 机身**左侧**，硬件锁存：按住 **0.5 s** 开机、长按 **2 s** 关机；休眠唤醒按右侧任意功能键（不是电源键） | **不接 MCU**，固件既看不见也控制不了 |
| 功能键 | 右侧 上 / 下 / OK 三键，共用一个 ADC 分压（GPIO0） | `board.h` 的 `APM_BTN_*`；上游 BSP 为 `BSP_BTN_COUNT 3` |
| 充电指示灯 | 绿色，由充电 IC 硬件直驱：充电常亮、充满熄灭 | 不可控、不可查，只能靠人眼看 |
| 电池 | 500 mAh（上游 BSP 标定用的是优特利 UTL 520 mAh），USB-C 充电 | CW2017 走标准 Battery Service；固件已写入上游 UTL 520 mAh 的 80 字节 profile（`device/main/battery.c`），SOC 真机实测有效（2026-11-06：3.96 V → 77–79%） |
| 屏幕 | ST7789P3 240×320 | 第一版不驱动，背光仅作低电量短闪 |

来源：FoloToy 官方上手文档 <https://ai-passport.folotoy.cn/guides/getting-started/>
（`board.h` 顶部也记了同一条，改硬件时两处一起改）。

### 功耗基线（**尚未测量**）

省电相关的固件改动已于 2026-11-07 落地并经过真机验证（见 [`ARCHITECTURE.md`](ARCHITECTURE.md)
的「功耗与省电」），但**没有任何一个场景量过电流**。下表先占位，量完填数：

| 场景 | 怎么进 | 平均电流 | 测得日期 |
| --- | --- | --- | --- |
| A 未连接广播 | 开机，不连 PC | — | — |
| B 已连接空闲 | GUI 连上，不开麦 | — | — |
| C 采集中 | 开麦录 60 s | — | — |

**已验的只有"配置真的生效"，不是"省了多少电"**（2026-11-07 真机）：
- `apm_link: 省电状态：低功耗时钟=main XTAL，modem sleep=已启用（ESP_OK）`；
- 连接参数两档都生效：空闲态 RTT 中位 117–118 ms vs 采集态 58.8 ms。

这两条说明开关打开了、行为变了，**但不等于电流下降了多少**。别把它们当省电数字引用。

口径：USB 电流表（或 PPK2）串在电池负极，设备**拔掉 USB 用电池供电**再量；
续航按 `500 mAh ÷ 平均电流` 估。**不要拿"插着 USB 时量到的电流"当结论**——
那里面混着充电电流，KI-007 那一轮就是因为这个绕了远路。

改任何一项省电配置都要重测这三个场景，并把数填进来。不填就别在别处写"省电了"。

### 关机态与"连不上"：现场排查第一条

**关机态下插 USB 只给充电 IC 上电（绿灯常亮），C3 的电源轨是断的。** PC 侧表现：

- `/dev/cu.usbmodem*` 不出现，`ioreg` 里没有 VID `0x303A` / PID `0x1001`；
- BLE 扫描 **0 个设备**——不是"连不上"，是设备根本没在广播。

绿灯只证明 VBUS 到位，**不证明设备已开机**（只充电、不走数据线的线一样会把灯点亮）。

排查顺序：**按电源键 0.5 s → 看绿灯 → 换一根能传数据的线**。2026-11-06 那次
"今早连不上"就是这条，不是电池故障也不是协议问题。

## macOS 安装 ESP-IDF

```bash
xcode-select --install

# 版本号写进目录名，避免升级时覆盖
git clone --branch v5.5.3 --recursive \
  https://git.espressif.com.cn/espressif/esp-idf.git \
  "$HOME/esp/esp-idf-v5.5.3"

# 中国大陆网络使用 Espressif 官方工具镜像
IDF_GITHUB_ASSETS=dl.espressif.cn/github_assets \
  "$HOME/esp/esp-idf-v5.5.3/install.sh" esp32c3
```

如果 `cmake` 或 `ninja` 不在 PATH，可以用 `uv tool install cmake` / `uv tool install ninja`，
或装 Homebrew 对应包；`scripts/check-env.sh` 会检查。

## 常见故障：python 小版本变了，ESP-IDF 起不来

`export.sh` 会去用 `~/.espressif/python_env/idf5.5_py3.<X>_env`，其中 `<X>` 是**安装时**
的 python3 小版本。系统 python 一升级（例如 brew 把 3.11 换成 3.13），这个 venv 就找不到，
典型报错：

```text
ERROR: ESP-IDF Python virtual environment ".../idf5.5_py3.13_env/bin/python" not found.
Please run the install script to set it up before proceeding.
```

本机 2026-11-04 就是这个状态：`~/.espressif/python_env` 里只有 `idf5.5_py3.11_env`，
而 `python3` 指向的是 WorkBuddy 内置的 `~/.workbuddy/binaries/python/versions/3.13.12`。
两条修复路径，先用 A 试：

```bash
# 方案 A（首选，零下载）：直接把 export.sh 指到已装好的旧 venv
export IDF_PYTHON_ENV_PATH="$HOME/.espressif/python_env/idf5.5_py3.11_env"
source ./scripts/activate-idf.sh      # 应输出 ESP-IDF v5.5.3
```

这条在本机验证过可用（`idf.py --version` → `ESP-IDF v5.5.3`）。前提是那个 venv 本身没坏；
它对应的基础 python 已经不在 PATH 里也没关系，venv 自带解释器。

```bash
# 方案 B（重装）：按当前 python3 新建 venv，并补齐 pip 依赖
"$HOME/esp/esp-idf-v5.5.3/install.sh" esp32c3
```

跑之前要知道它会做什么：`install.sh` 的参数就是目标芯片列表（`install.sh -h` 可查），
它会**新建** `idf5.5_py3.13_env` 并联网装 pip 依赖（几十个包，几百 MB 量级）；
交叉工具链（`riscv32-esp-elf`、`openocd-esp32` 等）已经装在 `~/.espressif/tools` 里，
版本匹配时会跳过下载。如果 IDF 5.5.3 对 python 3.13 的兼容性有问题，报错会出现在
pip 安装阶段——那就退回方案 A，或装一个 3.11 的解释器再让 `python3` 指向它。

判断标准只有一条：`idf.py --version` 输出里包含 `v5.5.3`。

> 方案 A 只对当前 shell 生效。想长期生效，把它写进自己的 shell 配置（本项目脚本不会
> 改你的 `.zshrc`），或者干脆用方案 B 重建一次。

## 激活与检查

```bash
./scripts/check-env.sh          # 只读体检：工具链 + ESP-IDF 版本
source ./scripts/activate-idf.sh # 只改当前 shell，不写 .zshrc，不替换已有 IDF
cd device && idf.py --version   # 必须包含 v5.5.3
```

## 基线自检

```bash
./scripts/verify.sh        # 结构 + 文档内链 + 脚本权限 + 协议单测 + 环境快照
./scripts/test-protocol.sh # 固件纯逻辑单测（CRC / 帧 / ADPCM + 省电判定，不需要 ESP-IDF）
./scripts/build-firmware.sh # 固件编译 + 合并镜像（要 ESP-IDF，约 1-2 分钟）
```

`verify.sh` 现在检查：必需目录与文件、文档内链、脚本可执行位、**协议单测**、环境快照。
`idf.py build` 故意不放进 verify（要完整 IDF 环境且耗时），改固件后单独跑
`build-firmware.sh`，并在交付里如实报告。

`check-env.sh` 和 `build-firmware.sh` 都会在「默认 venv 不存在」时，把
`IDF_PYTHON_ENV_PATH` 只读地指到磁盘上已有的 `idf5.5_py3.*_env`（对应上面的方案 A），
并在输出里标注 `venv 回退: ...`。这不会写任何文件；想长期生效仍然要自己 export。

## 虚拟声卡（PC 端落地时需要）

| 平台 | 方案 | 备注 |
| --- | --- | --- |
| macOS | BlackHole 2ch | 免费；Loopback 为付费替代 |
| Windows | VB-CABLE | 免费版够用 |

烧录与真机测试不在自动检查范围内；接硬件之前不要把任何音质指标标成"通过"。
