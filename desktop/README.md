# desktop/ —— PC 端软件（M2 部分落地）

把设备送上来的 BLE 音频解码后写入系统虚拟声卡，让会议软件把它当麦克风用。

**已实现**：协议层、BLE 链路、虚拟声卡输出（抖动缓冲）、`apmctl` CLI
（scan / status / record / sink / probe / analyze / selftest）、`apm-gui` 的 G0
（Tauri 骨架 + 蓝牙权限 + 设备扫描）。
**未实现**：GUI 的电平表与推流界面（G2）、按键注入、按键映射/音质报告界面（G4/G5）。
链路与声卡输出都在真机上跑通，并**在会议软件里实测可用**（用户确认：选中 BlackHole 2ch
当麦克风，说话有电平）。回环自动验收仍受限：宿主的麦克风权限被系统拒绝，读回来是数字静音。
macOS 上蓝牙**必须走 .app bundle 才能拿到权限**，见文末「macOS 上的蓝牙权限」。

## 技术栈

| 层 | 选择 | 状态 |
| --- | --- | --- |
| 语言 / 工具链 | Rust，`rust-toolchain.toml` 钉 1.95.0 | 已用 |
| 结构 | Cargo workspace（protocol / link / sink / app / cli / gui） | 已用 |
| BLE | `btleplug` 0.11 | 已用（真机 scan / status / record / sink 均通过） |
| 音频输出 | `cpal` 0.15 → 系统虚拟声卡（macOS BlackHole / Windows VB-CABLE） | 已实现 `crates/sink`，40–80 ms 抖动缓冲 |
| GUI | Tauri 2 + Vue 3 + Vite + TS | 已落地 G0（骨架 + 权限 + 设备发现） |

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
    app/              # apm-app：无 UI 应用内核（状态机 / 编排 / 事件 / 诊断日志）
      src/{core,state,events,log}.rs
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
```

`cargo test` 会读 `config/protocol-vectors.json`。那份文件由**固件侧 C 实现**导出：

```bash
./scripts/gen-protocol-vectors.sh          # 重新生成
./scripts/gen-protocol-vectors.sh --check  # 只检查是否过期
```

改了 `device/main/protocol.c` 就必须重跑生成脚本，否则对拍会用旧向量。

## GUI（apm-gui）—— 当前只到 G0

分期方案见 [`../docs/GUI.md`](../docs/GUI.md)。**G0 = 骨架 + 应用身份 + 蓝牙权限 + 设备发现**，
界面里还没有电平表与推流按钮（那是 G2），现在界面上明确标着"G2 接入"，不显示假数字。

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
./scripts/apmctl-macos.sh sink --to "BlackHole 2ch" --seconds 30  # 实时推进虚拟声卡
./scripts/apmctl-macos.sh sink --to "BlackHole 2ch" --tone --verify # 推正弦并回读
./scripts/apmctl-macos.sh probe --device 麦克风 --seconds 4        # 只读输入电平
apmctl analyze test.wav             # 分析 WAV 不碰蓝牙，可以直接跑
apmctl analyze test.wav --json      # 输出 ai-passport-mic-quality-report-1
```

`record` 会发 CTRL START 强制采集（不依赖 OK 键），结束时发 STOP。
序号跳变累计成"丢失块数"单独报出来 —— 不用"能听到声音"掩盖链路质量。

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
5. 不实现烧录、ASR、文字注入。

## 已知限制

- **读不到协商后的 MTU**：btleplug 不暴露这个值。固件侧会检查并按 STATUS 上报，
  PC 侧只能从音频块长度反推（`record` 遇到长度不是 165 的块会立刻警告并计数）。
- **声卡输出未做重采样**：设备是 16 kHz，声卡也必须原生支持 16 kHz，否则直接报错。
  BlackHole 2ch 支持（8 kHz–768 kHz），但要求声卡切到 16 kHz 会改变它的标称采样率。
- **`--verify` 在本机不可用**：探针要读输入，而本机给 WorkBuddy 启动的任何进程都不给
  麦克风权限（同一台机器上用 ffmpeg 录默认麦克风也是全静音）。当前靠人工看系统声音面板
  的电平条验收；等 GUI 用正规 bundle 申请权限后，`--verify` 才能回到自动验收。
- **时钟漂移没有被补偿**：设备 16 kHz 与主机 16 kHz 不同源，20 秒里累计约 84 ms 静音帧。
  自适应缓冲能保住不抖到断流，但根治要加漂移补偿（微调输出速率或按需插删采样）。
- **GUI 只到 G0**：界面能做设备扫描与连接，没有电平表、没有推流按钮、没有按键映射编辑。
- **编排还没全部进 `apm-app`**：`detect_adapter` / `scan` / `connect` 已在 `apm-app`，
  但 `record` / `sink` 的编排仍在 `crates/cli/src/cmd.rs`，`apm-app` 也还没依赖 `apm-sink`。
  这是 G2 的前置——不迁，GUI 和 CLI 会各长一套编排（见 `../docs/GUI.md` 决定 D1）。
- **裸二进制不能用蓝牙**：macOS TCC 要求 bundle + 用途声明，直接 `./target/release/apmctl record`
  会被系统 abort，见上文「macOS 上的蓝牙权限」。
- **一处偶发满量程毛刺**：6 次真机录音里有 2 次在 7.7–8.5 s 出现 ±32767 交替噪声（约 0.8 s），
  同一次里 `序号跳变丢失=0`，说明不是丢块，疑似 ADPCM 状态失配或 codec 侧瞬态。**未定位。**

## 验证状态

| 项 | 状态 |
| --- | --- |
| `cargo test`（protocol，31 项） | 通过；含 5 项与 C 实现的逐字节对拍 |
| `cargo build`（workspace） | 通过 |
| `apmctl selftest` | 通过（同一份向量，15 项） |
| `apmctl analyze` | 用合成 WAV 与真机 WAV 都跑过 |
| `scan` | **真机通过**：扫到 `AI Passport Mic`（RSSI -48 dBm，`本项目服务=yes`） |
| `status` | **真机通过**：连接成功，`state=ready ble=是 audio=是`，首个 STATUS 读失败但通知正常 |
| `record` | **真机通过**：15 s / 748 块 / 0 丢块；但 6 次里 1 次丢 177 块，属偶发 |
| `sink`（默认目标 80 ms） | 真机 20 s：999 块 / 0 丢块 / 设备侧 0 丢弃 / **0 欠载、0 静音帧**，缓冲稳在 68–76 ms |
| `sink`（60 s 长稳） | 2998 块 / 0 丢块；修复前的收尾欠载（停止时仍在消费）已改为「先停写再断链」 |
| 会议软件端到端 | **通过**（用户实测：会议软件选 `BlackHole 2ch`，说话时麦克风电平跟随） |
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
