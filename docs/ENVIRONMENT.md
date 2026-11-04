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

## 初始化基线自检

```bash
./scripts/verify.sh
```

当前阶段（还没有代码）它检查：必需目录与文件、文档内链、脚本可执行位、环境快照。
落地代码后再往里加 `cargo test` / `idf.py build` / 前端 typecheck，**不要**在没有代码时
假装这些检查已经通过。

## 虚拟声卡（PC 端落地时需要）

| 平台 | 方案 | 备注 |
| --- | --- | --- |
| macOS | BlackHole 2ch | 免费；Loopback 为付费替代 |
| Windows | VB-CABLE | 免费版够用 |

烧录与真机测试不在自动检查范围内；接硬件之前不要把任何音质指标标成"通过"。
