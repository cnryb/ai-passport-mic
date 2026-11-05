# desktop/ —— PC 端软件（M2 主链路 + GUI G1–G5）

把设备送上来的 BLE 音频解码后写入系统虚拟声卡，让会议软件把它当麦克风用。

**已实现**：协议层、BLE 链路、虚拟声卡输出（抖动缓冲）、`apmctl` CLI
（scan / status / record / sink / ping / probe / analyze / selftest）、`apm-gui` 的
G0–G5（骨架 + 权限 + 设备发现 + 状态机 + 实时电平与推流 + 声卡选择与会话 WAV +
音质报告 + 按键映射编辑）。音频编排只有一份：`crates/app`，CLI 与 GUI 都只调它。
**未实现**：G3b 回环自动验收（`--verify` 仍在 CLI）、G6 签名公证、按键注入（M3）、
GUI 的 `run_selftest`（bundle 里没有向量文件，协议自检仍归 `apmctl selftest`）。
链路与声卡输出都在真机上跑通，并**在会议软件里实测可用**（用户确认：选中 BlackHole 2ch
当麦克风，说话有电平）。回环自动验收仍受限：宿主的麦克风权限被系统拒绝，读回来是数字静音。
macOS 上蓝牙**必须走 .app bundle 才能拿到权限**，见文末「macOS 上的蓝牙权限」。

## 技术栈

| 层 | 选择 | 状态 |
| --- | --- | --- |
| 语言 / 工具链 | Rust，`rust-toolchain.toml` 钉 1.95.0 | 已用 |
| 结构 | Cargo workspace（protocol / link / sink / app / cli / gui） | 已用 |
| BLE | `btleplug` 0.11 | 已用（真机 scan / status / record / sink 均通过） |
| 音频输出 | `cpal` 0.15 → 系统虚拟声卡（macOS BlackHole / Windows VB-CABLE） | 已实现 `crates/sink`：40–80 ms 抖动缓冲 + 单样本漂移补偿 |
| GUI | Tauri 2 + Vue 3 + Vite + TS | 已落地 G0–G5；G3b/G6 未做（见 `../docs/GUI.md` §10） |

## 目录

```text
desktop/
  Cargo.toml
  rust-toolchain.toml
  crates/
    protocol/         # 帧、CRC、ADPCM、按键映射 schema、WAV 读写、音质指标
      src/{crc,frame,adpcm,keymap,wav,metrics}.rs
      tests/c_vectors.rs   # 读 C 侧导出的共享向量，逐字节对拍
    link/             # btleplug 封装：扫描 / 连接 / 订阅 / 通知解析
      src/device.rs
    sink/             # 抖动缓冲 + 写虚拟声卡（cpal），含回环探针
      src/lib.rs
    app/              # apm-app：无 UI 应用内核（状态机 / 编排 / 事件 / 诊断 / 报告 / 配置）
      src/core.rs     # 状态机 + 扫描/连接/推流编排 + 5 Hz telemetry
      src/session.rs  # 声卡线程（cpal Stream 是 !Send，只能线程内自持）+ WAV 增量写 + 指标
      src/pipeline.rs # 音频块 → 长度校验/序号跳变/解码 → 声卡或文件（CLI 与 GUI 唯一入口）
      src/report.rs   # 音质报告（与 apmctl analyze --json 同一份）
      src/{state,events,log,settings,keymap_store}.rs
    cli/              # apmctl
      src/{main,cmd}.rs
    gui/              # apm-gui：Tauri 2 主程序 + Info.plist 权限声明
      src/{main,commands}.rs
      tauri.conf.json Info.plist capabilities/
      frontend/       # Vue 3 + Vite + TS
```

## 构建与测试

```bash
cd desktop
cargo test            # protocol 的全部测试，含与 C 实现的逐字节对拍
cargo build --release # 产出 target/release/apmctl
../scripts/test-sink.sh  # 真声卡冒烟（默认 #[ignore]；没虚拟声卡就 SKIP）
```

`cargo test` 会读 `config/protocol-vectors.json`。那份文件由**固件侧 C 实现**导出：

```bash
./scripts/gen-protocol-vectors.sh          # 重新生成
./scripts/gen-protocol-vectors.sh --check  # 只检查是否过期
```

改了 `device/main/protocol.c` 就必须重跑生成脚本，否则对拍会用旧向量。

## GUI（apm-gui）—— G0–G5 已落地

分期方案见 [`../docs/GUI.md`](../docs/GUI.md)。界面四个 tab：实时（设备 / 电平表 / 指标条 /
声卡与缓冲 / 开始停止 / 同时保存 WAV）、音质报告、按键映射、诊断。

- 电平表是 canvas 手绘，数据来自后端 5 Hz `telemetry`；**前端不算任何指标**。
- 推流前会先检查有没有选声卡、声卡是否原生支持 16 kHz；不满足直接报错，不先开流再报错。
- 会话 WAV 默认不落盘，勾选后写到 `~/Library/Application Support/com.cnryb.ai-passport-mic/sessions/`。
- 按键映射页只改映射表；**注入层（M3）没实现，改完不会真的敲键盘**，页面里写明了。

```bash
./scripts/build-gui.sh                 # 前端构建 + tauri build --bundles app + 校验 Info.plist
open "desktop/target/debug/bundle/macos/AI Passport Mic.app"   # 必须经 LaunchServices 启动
```

几条硬规则，别绕：

1. **不能直接跑 `desktop/target/debug/apm-gui`。** macOS TCC 按"责任 app"判定，从没有用途
   声明的宿主（终端 / IDE）启动的进程调用 CoreBluetooth 会直接 abort。只有 `.app` 经
   `open` 启动，系统才认它是 `com.cnryb.ai-passport-mic` 并弹授权。
2. **bundle identifier 固定 `com.cnryb.ai-passport-mic`。** 改了等于让所有用户重新授权一次，
   `scripts/build-gui.sh` 会校验它。
3. **`cargo test` 不覆盖 GUI。** workspace 的 `default-members` 不含 `crates/gui`（tauri 400+
   依赖会把一次 `cargo test` 拖到十分钟），要单独 `cargo test -p apm-gui`。
4. **无人值守验收**：`open ... --args --smoke-scan` 会走同一套「检测适配器 → 扫描 6 s」代码
   路径，把结果写进诊断日志后退出（退出码 0 = 至少发现一个带本项目服务 UUID 的设备）。

诊断日志在 `~/Library/Logs/AI Passport Mic/app-YYYYMMDD.jsonl`（JSON Lines，一行一条），
界面「诊断」页可过滤、复制诊断包、打开目录。

**已知行为**：蓝牙授权弹窗没人点、或系统蓝牙关闭时，`btleplug` 会一直等适配器状态。
内核因此加了 6 s 探测超时（`ADAPTER_PROBE_TIMEOUT`），超时后状态落到 `no_adapter` 并
明确提示"授权弹窗没点 / 蓝牙没开"，不会让界面永远卡在"检测中"。

## apmctl

> **macOS 上不要直接跑 `./target/release/apmctl`。** 裸二进制调 CoreBluetooth 会被系统 abort，
> 必须经 `scripts/apmctl-macos.sh` 以 app bundle 身份启动（它会自动生成 bundle、签名、
> 并用 `--log` 把输出回传）。下面是等价的子命令写法。

```bash
./scripts/apmctl-macos.sh selftest                     # 用共享向量自检协议层，不需要硬件
./scripts/apmctl-macos.sh scan --timeout 6             # 扫附近的设备
./scripts/apmctl-macos.sh status --timeout 10          # 连接并打印 STATUS / 按键事件
./scripts/apmctl-macos.sh record test.wav --seconds 10 # 录 10 秒到 WAV，打印丢块与电平
./scripts/apmctl-macos.sh ping --count 20              # 链路往返时延（PING/PONG）
./scripts/apmctl-macos.sh sink --to "BlackHole 2ch" --seconds 30  # 实时推进虚拟声卡
./scripts/apmctl-macos.sh sink --to "BlackHole 2ch" --tone --verify # 推正弦并回读
./scripts/apmctl-macos.sh probe --device 麦克风 --seconds 4        # 只读输入电平
apmctl analyze test.wav             # 分析 WAV 不碰蓝牙，可以直接跑
apmctl analyze test.wav --json      # 输出 ai-passport-mic-quality-report-1
```

`record` 会发 CTRL START 强制采集（不依赖 OK 键），结束时发 STOP。
序号跳变累计成"丢失块数"单独报出来 —— 不用"能听到声音"掩盖链路质量。

`ping` 发 `0x04 PING`（递增 seq u32），设备原样回 `0x83 PONG`，输出 RTT 的
min / 中位 / p95 / max 与抖动（`--json` 出 `ai-passport-mic-ping-report-1`）。
它测的是**链路往返**：含两端协议栈、连接间隔（实测 30 ms）与设备事件任务调度，
音频单程 ≈ RTT/2 只能当上界估计，**不能**当端到端声学延迟用（见 KI-003）。

## 与固件的对齐方式

不靠"两份实现看起来一样"：

1. `device/test/gen_vectors.c` 用 C 实现跑出 CRC / 帧 / ADPCM 的字节结果，写成
   `config/protocol-vectors.json`。
2. `desktop/crates/protocol/tests/c_vectors.rs` 读同一份文件，逐字节比对。
3. ADPCM 还比对**跨块状态**（`state_before` / `state_after`），因为块头自同步是
   "丢块不污染后续音频"的前提，只比单块看不出来。

## 契约（M2 剩余部分仍要满足）

1. 协议实现与 [`../docs/ARCHITECTURE.md`](../docs/ARCHITECTURE.md) 一致。
2. 断连、未订阅、虚拟声卡不可用 → 明确状态；**不允许**继续向声卡写静音假装在录音。
   （断连与未订阅已在 STATUS 与 `record` 的错误路径里体现；声卡那项已随 `crates/sink`
   落地——`Sink::open` 打不开就失败并列出可用设备，GUI 侧的提示语留到 G3。）
3. 序号跳变计数并上报。
4. 按键注入失败或异常退出时，释放所有按下的修饰键（M3）。
5. 长期时钟漂移用单样本插删补偿，**不允许**靠周期性欠载或整帧丢弃兜底。
6. 满量程毛刺必须被静音替换后才推进声卡，不允许把 ±32767 交替噪声直接放出去。
7. 不实现烧录、ASR、文字注入。

## 已知限制

- **读不到协商后的 MTU**：btleplug 不暴露这个值。固件侧会检查并按 STATUS 上报，
  PC 侧只能从音频块长度反推（`record` 遇到长度不是 165 的块会立刻警告并计数）。
- **声卡输出未做重采样**：设备是 16 kHz，声卡也必须原生支持 16 kHz，否则直接报错。
  BlackHole 2ch 支持（8 kHz–768 kHz），但要求声卡切到 16 kHz 会改变它的标称采样率。
- **`--verify` 在本机不可用**：探针要读输入，而本机给 WorkBuddy 启动的任何进程都不给
  麦克风权限（同一台机器上用 ffmpeg 录默认麦克风也是全静音）。当前靠人工看系统声音面板
  的电平条验收；等 GUI 用正规 bundle 申请权限后，`--verify` 才能回到自动验收。
- **时钟漂移已补偿，但只在仿真里验过**：`crates/sink/src/playout.rs` 用单样本插删吸收长期
  漂移（比例控制，上限 1%）。仿真里 15933 Hz 生产 / 16000 Hz 消费跑 60 s 零欠载、零超容量
  丢弃；真机上"20 s 差 84 ms"这个数还没用新代码复测。
- **GUI 缺 G3b/G6**：回环自动验收仍在 CLI（`apmctl sink --verify`），签名只有 ad-hoc。
- **编排还没全部进 `apm-app`**：`detect_adapter` / `scan` / `connect` 已在 `apm-app`，
  编排已迁进 `apm-app`（`session` / `pipeline`），CLI 里只剩参数解析与打印，
  GUI 走同一条管线（见 `../docs/GUI.md` 决定 D1）。
- **裸二进制不能用蓝牙**：macOS TCC 要求 bundle + 用途声明，直接 `./target/release/apmctl record`
  会被系统 abort，见上文「macOS 上的蓝牙权限」。
- **满量程毛刺：根因未定位，只做了两端拦截**（`../docs/KNOWN_ISSUES.md` KI-001）。
  6 次真机录音里 2 次在 7.7–8.5 s 出现约 0.8 s 的 ±32767 交替噪声，同一次
  `序号跳变丢失=0`（不是丢块）。用编码→解码复现证明编解码自身最多污染一两块就恢复
  （`tests/glitch_repro.rs`），所以垃圾**在进 ADPCM 之前**就产生了。现在固件编码前压成
  静音（`apm_pcm_is_corrupt`），PC 端解码后再兜一层（`apm_protocol::glitch`）。
  **"为什么 I2S/ES8311 会读出这种数据"仍未定**，只是不再让它进会议软件；固件被压掉的
  块数只在串口日志里，没进 STATUS 载荷。
- **只有链路 RTT，没有端到端声学延迟**：`apmctl ping` 覆盖 BLE + 两端协议栈；
  codec 采集、20 ms 分块、抖动缓冲、声卡播放这几段还没进测量，卡在宿主麦克风权限
  （KI-003）。

## 验证状态

| 项 | 状态 |
| --- | --- |
| `cargo test`（workspace，63 项） | 通过；protocol 31 + 向量对拍 5 + 毛刺复现 3 + link 3 + sink 5 + app 13 + cli 4；无 warning |
| `cargo build`（workspace） | 通过 |
| `apmctl selftest` | 通过（同一份向量，15 项） |
| `apmctl analyze` | 用合成 WAV 与真机 WAV 都跑过 |
| `scan` | **真机通过**：扫到 `AI Passport Mic`（RSSI -48 dBm，`本项目服务=yes`） |
| `status` | **真机通过**：连接成功，`state=ready ble=是 audio=是`，首个 STATUS 读失败但通知正常 |
| `record` | **真机通过**：15 s / 748 块 / 0 丢块；但 6 次里 1 次丢 177 块，属偶发 |
| `sink`（默认目标 80 ms） | 真机 20 s：999 块 / 0 丢块 / 设备侧 0 丢弃 / **0 欠载、0 静音帧**，缓冲稳在 68–76 ms |
| `sink`（60 s 长稳） | 2998 块 / 0 丢块；修复前的收尾欠载（停止时仍在消费）已改为「先停写再断链」 |
| 会议软件端到端 | **通过**（用户实测：会议软件选 `BlackHole 2ch`，说话时麦克风电平跟随） |
| 毛刺抑制（单测 + 合成 WAV） | 通过：`crates/protocol` 5 项单测 + `analyze` 正确识别 1 段 / 799.6 ms 并判 fail |
| 漂移补偿（仿真 60 s） | 通过：±5000 ppm 两个方向均 0 欠载 / 0 超容量丢弃，ppm 估计误差 < 400 |
| 上述两项的**真机复测** | **未做**（需要重新烧录固件 + 跑 60 s `sink`），见 KI-002 |
| `ping`（单测 + 统计口径） | 代码通过：4 项统计单测（最近秩 p95 / 抖动按发送顺序 / 单样本）；**真机未测**，需烧新固件 |
| `sink --tone` | 声卡流正常消费（480000 帧写入）；**用户在「系统设置 → 声音 → 输入」看到电平条随信号跳动**，证明进程→声卡这一段通了 |
| `probe` | 内置麦克风与 BlackHole 输入端都读到 -120 dBFS（1 LSB 抖动）→ 麦克风权限被拒，探针不可用 |
| GUI 构建（`scripts/build-gui.sh`） | **通过**：`.app` 28.3 MiB，ad-hoc 签名有效，Info.plist 内含 `NSBluetooth*` / `NSMicrophone*`，bundle id `com.cnryb.ai-passport-mic` |
| GUI 启动（`open` 启动、无 TCC abort） | **通过**：进程存活、webview 已加载、诊断日志有启动记录 |
| GUI 适配器探测超时保护 | **通过**：授权未点/蓝牙关闭时 6 s 落到 `no_adapter` 并给出可执行提示，不再无限挂起 |
| GUI 首次蓝牙授权 + 扫描到设备 | **未验证**：需要人在系统弹窗点「允许」。本轮两次启动都停在等待授权（屏幕处于锁屏/休眠，无法确认弹窗是否被点） |

## macOS 上的蓝牙权限（实测坑）

btleplug 走 CoreBluetooth，受 TCC 管控。**裸 CLI 二进制调用 CoreBluetooth 会被系统直接 abort**：

```text
termination: namespace=TCC
This app has crashed because it attempted to access privacy-sensitive data
without a usage description. The app's Info.plist must contain an
NSBluetoothAlwaysUsageDescription key ...
```

而且 TCC 按「责任 app」判定：从没有该声明的宿主（WorkBuddy、Terminal.app、Warp 都没有）启动
的进程，即使二进制里嵌了 `__TEXT,__info_plist` 也照样崩。可用的跑法只有一种：

1. 把 `apmctl` 放进带 `NSBluetoothAlwaysUsageDescription` 的 `.app` bundle；
2. 用 LaunchServices（`open`）启动它，让系统弹一次「允许使用蓝牙」；
3. 需要回收 stdout 时，用一个原生启动器 app 托管子进程并把输出重定向到文件。

所以 `record` / `status` 目前不能直接 `./apmctl record` 跑。等 GUI（Tauri）落地，这个问题会自然消失；
在那之前，脚本化验证必须走 bundle 通道，或者把这段记进测试手册。
