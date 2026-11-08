# AGENTS.md —— 仓库规则（AI 协作入口）

本文件是仓库根的唯一入口，只做路由与硬规则。产品是什么、怎么用，看
[`README.md`](README.md)；设计细节看 [`docs/`](docs/)。

## 当前状态（务必先接受这个事实）

本仓库是 **AI Passport Mic**：固件落到 M1 并已在真机上跑通采集与 BLE 链路；
桌面端 M2 主链路（BLE → 解码 → 虚拟声卡）已实测可用，GUI 落到 G1–G5（代码完成，
真机验收未做）。

- 有：`docs/`、`config/`（按键映射 + **跨端协议向量**）、`scripts/`、`device/`（ESP-IDF 工程）、
  `desktop/`（Rust workspace：protocol / link / sink / app / cli / gui）。
- **有**：GUI 的 G1–G5——状态机、实时电平与推流、声卡选择与会话 WAV、音质报告、按键映射编辑。
- **有**：按键注入（M3，macOS：CGEvent 点按 + 采集钩子 `capture.start` / `capture.stop`，
  左右 Alt `lalt` / `ralt`，**设备按键直连**——OK 键开关采集、up/down 注入映射键、
  切换窗口防抖，映射页内置辅助功能权限引导横幅；**真机端到端 2026-11-06 已验 PASS**）。
  未做的只有 **hold（按住不放）**：固件侧 PRESS/RELEASE 齐，缺 PC 端消费 RELEASE。
- **有**：设备屏幕（ST7789P3 240×320）——显示电量与三键功能。**2026-11-08 真机验过**：
  面板/背光/LEDC 初始化、30 s 自动熄屏（实测 30.2 s）、PC 下发按键标签（`SET_KEY_LABELS`）。
  亮灭手势：**只有长按 UP/DOWN 切屏**，单击不响应，开麦键的任何手势都不切屏；
  **屏幕与采集状态完全解耦**（开麦不点亮、采集中照常熄屏，因为"省电更重要"）。
  屏幕画面对不对（反色/镜像/观感）与 ST7789 是否真进 Sleep In 仍需人眼/量电流。
  详见 [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) 的「设备屏幕」节。
- **电源键不接 MCU**（硬件锁存：按住 0.5 s 开机、长按 2 s 关机），固件看不见它——
  **"按电源键亮屏"在这块板上做不到**，已用长按 UP/DOWN 替代。
- **没有**：G3b 回环自动验收（`--verify` 仍在
  `apmctl`）、G6 签名公证（只有 ad-hoc）、GUI 的 `run_selftest`、Windows 注入（SendInput）。
- **编排只有一份**：`detect_adapter` / `scan` / `connect` / `start_stream` / `stop_stream`
  / 采集解码写声卡全在 `apm-app`（`core` / `session` / `pipeline`）。`cli` 与 `gui` 都只调它，
  「录 WAV 并分析」用 `apmctl` 或界面都行，走的是同一条管线与同一份报告。
- 门禁现状（不要假设更多）：
  - `./scripts/verify.sh` —— 结构 / 文档内链 / 脚本权限 / **固件主机侧单测** /
    **屏幕点阵字库新鲜度** / 向量新鲜度 / `cargo test` / 环境快照。
  - `./scripts/test-protocol.sh` —— 固件**纯逻辑模块**的主机侧单测，不需要 ESP-IDF：
    `protocol.c`（CRC / 帧 / ADPCM / 毛刺判据 / PING-PONG / `SET_KEY_LABELS`，71 项）+
    `power_policy.c`（连接参数两档 / `LINK_SLOW` 判据 / 采集态状态机 / 发射功率滞回，65 项）+
    `screen_render.c` + `screen_state.c`（渲染 / 亮灭状态机，71 项）。
    **新写的固件判定逻辑应当尽量放进这类不依赖 ESP-IDF 的模块**，否则它只能在真机上"看一眼"。
  - `./scripts/gen-protocol-vectors.sh [--check]` —— 从 C 实现导出跨端向量。
  - `./scripts/gen-font-subset.py [--check]` —— 从 `device/main/screen_texts.h` 提取字符集，
    生成 16×16 点阵字库 `device/main/font16_data.h`（源字体 GNU Unifont，双许可 GPLv2+/OFL 1.1，
    只嵌渲染位图）。**改了屏幕文案必须重跑**，否则 `verify.sh` 会 FAIL ——
    漏了的后果是屏幕上一个方框，只有上真机才看得见。**在** verify 里。
  - `./scripts/build-firmware.sh` —— `idf.py build` + 合并镜像。**不在** verify 里（耗时），
    改固件后自己跑一次并在交付里如实报告。
  - `./scripts/build-gui.sh` —— 前端构建 + `tauri build --bundles app` + 校验 Info.plist
    的蓝牙/麦克风用途声明。**不在** verify 里（首次要拉 400+ crate）。
  - `./scripts/test-sink.sh` —— 真声卡冒烟（`session_with_real_sink`，默认 `#[ignore]`）。
    没装虚拟声卡时明说 SKIP，不假装测过。**不在** verify 里（要硬件）。
  - `./scripts/test-gui-perf.sh` —— G2 性能三项：**release 包**跑 90 s，量 CPU/内存/前端帧率。
    自动用 `--smoke-perf` 入口开流，按 PID 差值 + comm 白名单锁定 apm-gui 与 WebKit helper。
    **不在** verify 里（要硬件 + GUI）。debug 包数据不能当验收。
  - `./scripts/test-longrun.sh` —— G2/KI-006 长测：真机跑 **3 轮 × 600 s**（带 `--rtt`），
    按**多轮中位数**判欠载、按**每一轮**判链路干净，并出可粘贴的报告。
    **判据 2026-11-07 晚起为多轮中位数**（单轮方差极大，见 KI-006）；
    `--from-logs "logs/*.log"` 可换判据复核历史跑分，不必重跑。
    **不在** verify 里（要硬件 + 30 分钟）。
  - `cargo test` 用 workspace 的 `default-members`，**不含** `crates/gui`；GUI 要单跑
    `cargo test -p apm-gui` 或 `./scripts/build-gui.sh`。
  - 最近一次全绿（2026-11-08 复核）：`verify.sh` 八项全 PASS（固件主机侧单测 207 passed：
    协议 71 + 省电 65 + 屏幕 71；字库新鲜度 PASS）、
    `cargo test` **162 passed / 0 failed / 2 ignored**（7 个有测试的目标）、
    `clippy --all-targets` **0 警告**、`vue-tsc` 与 `npm run build` 通过、
    `build-gui.sh` 出包且 Info.plist 校验通过、
    `test-sink.sh` 在有 BlackHole 的机器上 PASS。门禁明细见
    [`docs/GUI.md`](docs/GUI.md) §14 与 [`docs/GUI_IMPLEMENTATION_LOG.md`](docs/GUI_IMPLEMENTATION_LOG.md) §14.2
    （那份是 2026-11-05 的快照：93 passed / `.app` 30.29 MiB）。
    **这些只说明"编得过、单测过、打得出来"，不说明 G1–G5 已验收**——真机项仍要人接设备、点授权。

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
          GUI = Tauri 2 + Vue 3，G0–G6 已到 G5，G3b/G6 未做）。
config/   按键映射 schema 与样例。
docs/     产品、架构、环境、音质、路线图、参考实现、GUI 方案（含实施记录 GUI_IMPLEMENTATION_LOG.md）、
          已知问题（KNOWN_ISSUES.md）。
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
./scripts/test-protocol.sh       # 固件纯逻辑单测单独跑（协议 + 省电逻辑，不需要 ESP-IDF）
./scripts/gen-protocol-vectors.sh --check  # 跨端向量是否与 C 实现一致
./scripts/build-firmware.sh      # 固件编译 + 合并镜像（需要 ESP-IDF，约 1-2 分钟）
./scripts/build-gui.sh           # 前端构建 + 打包 .app + 校验 Info.plist 用途声明
./scripts/test-sink.sh           # 真声卡冒烟（默认 #[ignore]），没虚拟声卡就明说 SKIP
./scripts/test-gui-perf.sh       # G2 性能三项：release 包 90 s，量 CPU/内存/前端帧率
./scripts/test-longrun.sh        # G2/KI-006 长测：真机 3×600 s，按多轮中位数判欠载 + 每轮判链路干净
./scripts/apmctl-macos.sh scan   # macOS 上跑 apmctl 的唯一正道（bundle + 蓝牙权限）
./scripts/apmctl-macos.sh ping --count 20  # 链路往返时延（PING/PONG），不是端到端声学延迟
cd desktop && cargo test         # 桌面端测试，含与 C 实现的逐字节对拍
```

CI 只有一条：`.github/workflows/macos-build.yml`。它**不新增任何判定逻辑**——
门禁直接调 `verify.sh`（外加 `clippy -D warnings` 与前端 typecheck/build），
打包直接调 `build-gui.sh --release`。所以本地能过 ≠ CI 能过的地方只有两处：
① ESP-IDF 在容器里（ubuntu + `espressif/idf:v5.5.3`），
② macOS 侧是干净 checkout（无 `sdkconfig`/`build/`）。
PR 只跑门禁；固件与双架构打包只在 **v* tag** 或手动触发时跑。
产物是 **ad-hoc 签名、未公证**（G6 未做），别把它当可分发包。

交付时按下面格式报状态，不要合并成一句话：

```text
Structure: PASS / FAIL
Docs links: PASS / FAIL
Protocol (C): PASS / FAIL（几项通过）
Font subset: PASS / FAIL（字库是否与 screen_texts.h 的文案一致）
Vectors: PASS / FAIL（是否与 C 实现一致）
Desktop (cargo test): PASS / FAIL / SKIP
GUI build (.app + Info.plist): PASS / FAIL / SKIP
GUI (G0-G6): PASS / FAIL / SKIP（写当前真实到哪一期，不要写"GUI 好了"）
Firmware build: PASS / FAIL / SKIP（IDF 不可用时写 SKIP，不要写成 PASS）
Env: PASS / FAIL（缺失项列出）
Unverified: 尚未验证的能力（**屏幕画面观感（反色/镜像）、ST7789 是否真的进了 Sleep In、
  背光电流收益**、三键电压窗口万用表复核、hold 按住不放、端到端声学延迟、
            G2 的 CPU/内存两项（实测 14.2%/208MB，均超口径；前端 59.8fps 已 PASS。
            **电平表的每帧成本已于 2026-11-07 优化**：静态层/动态层拆两张 canvas，
            CDP 实测主线程 ScriptDuration −42%、TaskDuration −12%，逐像素等价；
            但 **G2 的 CPU/内存未重测**，那两个数字仍是旧的，见 GUI.md G2 与
            GUI_IMPLEMENTATION_LOG.md §14.26）、G4 音质场景矩阵、KI-005 慢间隔自愈与"拦得住"那一半、
            M6 空片首烧与中断恢复；
            毛刺根因未定位见 docs/KNOWN_ISSUES.md KI-001，
            长测仍欠载见 KI-006——**根因已查明：macOS CoreBluetooth 攒批交付**
            （相邻块 <5ms 占 38–43%，而"正常 20ms 一块"占 0%；尖峰期 RTT 0.57–1.00× 平稳），
            且 2026-11-08 又修掉一个放大因子（**开场攒批被漂移补偿读成时钟漂移、按虚假
            ppm 删真实音频**，删 9406 样本 → 欠载 96；改 `arm_drift_base()` 后 4 轮 × 600 s
            中位 **3.9 次/分 → PASS**，但个别场次仍会短暂虚高）。**判据 2026-11-07 晚起为
            多轮中位数**（单轮方差极大：同条件跨 1.0 → 11.7 次/分），别拿单轮数字下结论。
            **功耗三个场景的电流一个都没量**（ENVIRONMENT.md 的基线表仍是空的），
            所以不要在任何地方写"省电了/省了多少电"；剩余真机项是 G2 的 CPU 与内存超标，
            见 GUI.md G2）
```
