# AGENTS.md —— 仓库规则（AI 协作入口）

本文件是仓库根的唯一入口，只做路由与硬规则。产品是什么、怎么用，看
[`README.md`](README.md)；设计细节看 [`docs/`](docs/)。

## 当前状态（务必先接受这个事实）

本仓库是 **AI Passport Mic**：固件落到 M1 并已在真机上跑通采集与 BLE 链路，桌面端落到 M2 的一部分。

- 有：`docs/`、`config/`（按键映射 + **跨端协议向量**）、`scripts/`、`device/`（ESP-IDF 工程）、
  `desktop/`（Rust workspace：protocol / link / sink / app / cli / gui）。
- **没有**：按键注入、GUI 的电平表与推流界面（G2）。GUI 只到 G0：骨架 + 蓝牙权限 + 设备发现。
  所以"录 WAV 并分析""推虚拟声卡"靠 `apmctl`，"点界面连设备"靠 `apm-gui`。
- **编排只迁了一半**：`detect_adapter` / `scan` / `connect` 已在 `apm-app`，
  `record` / `sink` 仍在 `crates/cli/src/cmd.rs`（`apm-app` 尚未依赖 `apm-sink`）。
  GUI 的 G2 之前必须迁完，否则两个前端各长一套编排。
- 门禁现状（不要假设更多）：
  - `./scripts/verify.sh` —— 结构 / 文档内链 / 脚本权限 / 协议单测 / 向量新鲜度 /
    `cargo test` / 环境快照。
  - `./scripts/test-protocol.sh` —— 只验固件协议层，不需要 ESP-IDF。
  - `./scripts/gen-protocol-vectors.sh [--check]` —— 从 C 实现导出跨端向量。
  - `./scripts/build-firmware.sh` —— `idf.py build` + 合并镜像。**不在** verify 里（耗时），
    改固件后自己跑一次并在交付里如实报告。
  - `./scripts/build-gui.sh` —— 前端构建 + `tauri build --bundles app` + 校验 Info.plist
    的蓝牙/麦克风用途声明。**不在** verify 里（首次要拉 400+ crate）。
  - `cargo test` 用 workspace 的 `default-members`，**不含** `crates/gui`；GUI 要单跑
    `cargo test -p apm-gui` 或 `./scripts/build-gui.sh`。

## 硬规则

1. **不整份拷贝上游仓库。** 特别是不要把 FoloToy AI Passport 的完整 fork、或任何既有
   VibeMic 应用整目录搬进来。参考实现在 [`docs/REFERENCES.md`](docs/REFERENCES.md) 里
   列明来源与「可以借什么、不可以借什么」。
2. **结构与范围以 [`docs/PRODUCT.md`](docs/PRODUCT.md) 为准。** 第一版只做无线麦克风
   （BLE → PC → 虚拟声卡），不做 ASR、不做文字注入、不做 AI Agent。要加范围先改文档。
3. **改协议先改 [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)。** 帧格式、UUID、
   块长、延迟预算是跨端契约，代码必须与文档一致，不允许"先写代码再补文档"。
4. **落地代码时同步更新文档。** 新增 crate/组件后，更新对应子目录 README 与根 README
   的目录表；不允许出现"文档说有、仓库里没有"的情况（这正是本次清理要修的问题）。
5. **验证要如实。** 区分「已验证」与「未验证」：编译通过 ≠ 真机通过；没接硬件就不能
   写"音质合格"。
6. **不提交敏感信息。** 凭据、设备二维码密钥、私钥、个人信息、未脱敏日志一律不入库。
7. **提交与推送只在用户要求时做。** 先 `git status --short --branch`，保留用户已有改动。

## 落代码时的目录约定

```text
device/   固件（ESP-IDF 5.5.3、ESP32-C3）。落地后在此目录内自带 AGENTS.md 管子树。
desktop/  PC 软件（Rust workspace：protocol / link / sink / app / cli / gui；
          GUI = Tauri 2 + Vue 3，当前只到 G0）。
config/   按键映射 schema 与样例。
docs/     产品、架构、环境、音质、路线图、参考实现、GUI 方案。
scripts/  环境与校验入口。
```

`desktop/crates/app`（`apm-app`）是**无 UI 应用内核**，GUI 与 CLI 都只能调它，
不允许在 `crates/gui` 或 `crates/cli` 里各写一套编排（见 `docs/GUI.md` 的决定 D1）。

两个子树的规则互不通用：`device/` 的 ESP-IDF/LVGL/烧录约定不适用于 `desktop/`，
反之亦然。子目录一旦建立自己的 `AGENTS.md`，以子树内那份为权威。

## 当前验证入口

```bash
./scripts/check-env.sh           # 只读环境体检（工具链 + ESP-IDF v5.5.3）
source ./scripts/activate-idf.sh # 当前 shell 激活 ESP-IDF
./scripts/verify.sh              # 基线自检：结构、内链、权限、协议单测、向量、cargo test、环境
./scripts/test-protocol.sh       # 固件协议单测单独跑（不需要 ESP-IDF）
./scripts/gen-protocol-vectors.sh --check  # 跨端向量是否与 C 实现一致
./scripts/build-firmware.sh      # 固件编译 + 合并镜像（需要 ESP-IDF，约 1-2 分钟）
./scripts/build-gui.sh           # 前端构建 + 打包 .app + 校验 Info.plist 用途声明
./scripts/apmctl-macos.sh scan   # macOS 上跑 apmctl 的唯一正道（bundle + 蓝牙权限）
cd desktop && cargo test         # 桌面端测试，含与 C 实现的逐字节对拍
```

交付时按下面格式报状态，不要合并成一句话：

```text
Structure: PASS / FAIL
Docs links: PASS / FAIL
Protocol (C): PASS / FAIL（几项通过）
Vectors: PASS / FAIL（是否与 C 实现一致）
Desktop (cargo test): PASS / FAIL / SKIP
GUI build (.app + Info.plist): PASS / FAIL / SKIP
GUI (G0-G6): PASS / FAIL / SKIP（写当前真实到哪一期，不要写"GUI 好了"）
Firmware build: PASS / FAIL / SKIP（IDF 不可用时写 SKIP，不要写成 PASS）
Env: PASS / FAIL（缺失项列出）
Unverified: 尚未验证的能力（按键手势逐项核对、延迟、GUI 首次授权与推流）
```
