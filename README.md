# AI Passport Mic

把 FoloToy AI Passport（ESP32-C3）做成一个**纯无线麦克风**：设备采集声音，通过 BLE 送到
PC，由 PC 输出到系统虚拟声卡，会议软件选择该虚拟声卡作为麦克风。第一版不包含语音识别、
文字输入或云端 ASR。

## 当前状态

主链路已经能用，**验收没做完**。分项如下：

| 部分 | 状态 |
| --- | --- |
| 固件（[`device/`](device/)） | M1 真机跑通：ES8311 16 kHz 采集、IMA-ADPCM、自定义 GATT、单 ADC 三键判定（2026-11-06 逐项核对 PASS）；省电配置已生效，**但电流未实测** |
| 桌面主链路（[`desktop/`](desktop/)） | M2 真机跑通：BLE → ADPCM 解码 → 虚拟声卡，会议软件里实测可当麦克风 |
| GUI | G0–G5 编码完成，部分真机验收：状态机、实时电平与推流、声卡选择与会话 WAV、音质报告、按键映射编辑 |
| 按键注入（M3） | macOS 点按 + 设备按键直连，2026-11-06 真机端到端 PASS |
| 开箱烧写（M6） | `apmctl firmware ports\|probe\|flash` + GUI「设备烧写」页；真机写入与 `FlashMd5` 校验已通（2026-11-06） |
| 未做 | G3b 回环自动验收、G6 签名公证、GUI `run_selftest`、hold（按住不放）、Windows 注入、运行期自动重连 |

**尚未验证，不要当成通过**（完整清单在 [`docs/TODO.md`](docs/TODO.md)，证据链在
[`docs/KNOWN_ISSUES.md`](docs/KNOWN_ISSUES.md)）：

- **端到端声学延迟**：只有链路 RTT，`apmctl ping` 不能替代，见 KI-003。
- **G2 性能**：CPU 14.2%（口径 < 8%）、内存 208 MB（口径 < 150 MB）超标；前端 59.8 fps
  达标；GUI 侧还没按新的欠载口径重跑。
- **功耗**：广播 / 空闲 / 采集中三个场景的电流一个都没量，所以不要写"省了多少电"。
- **G4 音质场景矩阵**、**M6 空片首烧与「烧写中断后重插恢复」**、GUI 烧写页真机走查。
- **KI-001** 满量程毛刺：两端已拦截，**根因未定位**；**KI-005** 两条护栏的"拦得住脏会话"
  那一半仍只到单测；**KI-006** 多数场次达标，个别场次仍会短暂虚高。
- **三键电压窗口**：只验过分类结果，没上过万用表复核实际电压。

仓库约定（含交付时怎么分项报状态）见 [`AGENTS.md`](AGENTS.md)。

## 第一版边界

| 能力 | 决定 |
| --- | --- |
| 音频传输 | BLE 自定义 GATT，16 kHz / 16-bit / mono，IMA-ADPCM，20 ms 一块 |
| PC 输出 | 写入系统虚拟声卡（macOS: BlackHole / Loopback），不做系统音频驱动 |
| 设备按键 | UP / DOWN / OK 各一条映射（`keymap-4`）；**恰好一个键**绑 `ptt-toggle`（默认 OK，可换 UP/DOWN），按下切换采集 |
| 采集钩子 | 开麦 / 关麦各注入一次可配置快捷键（点按，需辅助功能权限） |
| 忘关麦兜底 | 连续采集超时自动停，默认 10 min，界面可调 1–1440 min |
| 音质评估 | WAV 客观指标 + 固定场景录音 + 主观评分 + 延迟/稳定性记录 |
| 固件烧写 | 走 USB（C3 内置 USB-Serial-JTAG，免驱、不用按 BOOT）；只写 bootloader / 分区表 / app 三段，**不擦 NVS**；写完用 `FlashMd5` 校验 |

**不在第一版范围**：本地/云端语音识别、自动打字、AI Agent、Wi-Fi 推流、HFP 与标准蓝牙
耳机麦克风协议、系统音频驱动、iOS/Android App、Windows 首发。

## 目录

| 路径 | 内容 |
| --- | --- |
| [`device/`](device/) | 固件（ESP-IDF 5.5.3 / ESP32-C3 / 8 MB Flash / 无 PSRAM），见 [`device/README.md`](device/README.md) |
| [`desktop/`](desktop/) | Rust workspace：protocol / link / sink / app / cli / gui；GUI 为 Tauri 2 + Vue 3，见 [`desktop/README.md`](desktop/README.md) |
| [`config/`](config/) | 按键映射样例 [`button-map.example.json`](config/button-map.example.json) 与字段说明 [`config/README.md`](config/README.md)、跨端协议向量 `protocol-vectors.json`（由 C 实现导出） |
| [`docs/`](docs/) | [索引](docs/README.md) · [产品](docs/PRODUCT.md) · [架构](docs/ARCHITECTURE.md) · [环境](docs/ENVIRONMENT.md) · [音质](docs/SOUND_QUALITY.md) · [路线图](docs/ROADMAP.md) · [参考实现](docs/REFERENCES.md) · [GUI 方案](docs/GUI.md) · [GUI 实施记录](docs/GUI_IMPLEMENTATION_LOG.md) · [已知问题](docs/KNOWN_ISSUES.md) · [待办清单](docs/TODO.md) · [发布](docs/PUBLISHING.md) |
| [`scripts/`](scripts/) | 环境体检、IDF 激活、基线自检、协议单测与向量导出、固件构建、GUI 打包、性能与长测、macOS app-bundle 运行器 |
| [`.github/workflows/`](.github/workflows/) | CI：PR 跑门禁（`verify.sh` + clippy + 前端），v* tag 或手动触发时在 ESP-IDF 容器里编固件、双架构打 macOS 包并发 Release |
| [`AGENTS.md`](AGENTS.md) | 仓库规则与交付口径（AI 协作入口） |

## 上手

```bash
# 固件：需要 ESP-IDF v5.5.3
./scripts/check-env.sh              # 只读体检，不改任何文件
source ./scripts/activate-idf.sh    # 在当前 shell 激活 ESP-IDF
./scripts/build-firmware.sh         # idf.py build + 合并镜像

# 桌面端：产出 apmctl 与 .app（.app 里带随包固件）
cd desktop && cargo build --release && cd ..
./scripts/build-gui.sh

# macOS 上启动 GUI 必须经 LaunchServices，否则 CoreBluetooth 会被 TCC 直接 abort
open "desktop/target/debug/bundle/macos/AI Passport Mic.app"
```

**要碰蓝牙的子命令不能直接跑裸二进制**，必须走 bundle 包装（原因见
[`desktop/README.md`](desktop/README.md) 的「macOS 上的蓝牙权限」）：

```bash
./scripts/apmctl-macos.sh scan --timeout 6
./scripts/apmctl-macos.sh record test.wav --seconds 10
./scripts/apmctl-macos.sh sink --to "BlackHole 2ch" --seconds 30

# 纯离线命令直接跑裸二进制，不需要 bundle
./desktop/target/release/apmctl selftest
./desktop/target/release/apmctl analyze test.wav --json
```

宿主工具链的**实测版本以 `./scripts/check-env.sh` 输出为准**（写死在文档里必然漂移）。
已知坑：ESP-IDF 的 python venv 与 python 小版本绑定，找不到时 `check-env.sh` 会做一次
只读回退，手动修复见 [`docs/ENVIRONMENT.md`](docs/ENVIRONMENT.md) 的「方案 A」。

## 验证入口

| 命令 | 验什么 | 在 `verify.sh` 里 |
| --- | --- | --- |
| `./scripts/verify.sh` | 结构 / 文档内链 / 脚本权限 / 固件主机侧单测 / 向量新鲜度 / `cargo test` / 环境快照 | 就是它本身 |
| `./scripts/test-protocol.sh` | 固件纯逻辑单测（协议 71 项 + 省电逻辑 65 项 + 屏幕 71 项，合计 207），不需要 ESP-IDF | 是 |
| `./scripts/gen-protocol-vectors.sh --check` | 跨端向量是否仍与 C 实现一致 | 是 |
| `cd desktop && cargo test` | 桌面端测试，含与 C 实现的逐字节对拍（**不含** `crates/gui`） | 是 |
| `./scripts/build-firmware.sh` | 固件编译 + 合并镜像（约 1–2 分钟） | 否 |
| `./scripts/build-gui.sh` | 前端构建 + `tauri build` + 校验 Info.plist 用途声明 | 否 |
| `./scripts/test-sink.sh` | 真声卡冒烟；没装虚拟声卡就明说 SKIP | 否 |
| `./scripts/test-gui-perf.sh` | G2 性能三项：release 包跑 90 s，量 CPU / 内存 / 前端帧率 | 否 |
| `./scripts/test-longrun.sh` | G2 / KI-006 长测：真机 3 轮 × 600 s，按多轮中位数判欠载 | 否 |
| `cd desktop && cargo test -p apm-gui` | GUI 自己的测试（`default-members` 不含它） | 否 |

后四项要硬件或重依赖，所以不进 `verify.sh`。**"编得过、单测过、打得出来"不等于验收通过**，
交付时按 [`AGENTS.md`](AGENTS.md) 的格式分项报状态。

## 声音质量评估

音质不能只看"能听见"。方法与阈值见 [`docs/SOUND_QUALITY.md`](docs/SOUND_QUALITY.md)：

- 语音 RMS 建议 `-30 ~ -12 dBFS`，峰值不高于 `-1 dBFS`。
- 削波样本比例小于 `0.1%`，直流偏置绝对值小于满量程的 `1%`。
- 记录 ADPCM 序号丢块数；稳态端到端延迟目标 p95 ≤ `150 ms`。
- 安静房间、办公室、背景音乐三种环境，20 cm / 50 cm / 1 m 三种距离，0° / 45° / 背向三种角度。

## 技术架构

```text
AI Passport (ESP32-C3)
  microphone -> 16 kHz PCM -> IMA-ADPCM -> BLE GATT notify
                                      \-> BLE GATT event（按键 press/…，不是 HID）
                                                   |
                                                   v
PC: 设备链路 -> ADPCM 解码 -> 虚拟声卡 -> 会议软件
                            \-> WAV 录制 / 音质报告
```

协议帧、BLE 服务 UUID、延迟预算与功耗取舍见 [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)。
这些是**跨端契约**：改协议先改文档，再改两端代码并让 `test_protocol.c` 覆盖到。

## 开发约定

1. **不整份拷贝上游仓库。** FoloToy AI Passport 固件与既有 VibeMic 实现只作设计参考，
   来源与「可以借什么、不可以借什么」见 [`docs/REFERENCES.md`](docs/REFERENCES.md)。
2. **范围以 [`docs/PRODUCT.md`](docs/PRODUCT.md) 为准。** 加范围先改文档。
3. **落地代码时同步更新文档**：新增 crate / 组件后更新对应子目录 README 与本文件目录表，
   不允许"文档说有、仓库里没有"。
4. **验证要如实**：区分「已验证」与「未验证」，编译通过不等于真机通过，没接硬件不许写音质结论。
5. **不提交敏感信息**：凭据、设备密钥、私钥、个人信息、未脱敏日志一律不入库。

## 许可证

仓库整体采用 MIT 许可证（见 [`LICENSE`](LICENSE)）。若日后参考或复用上游
FoloToy AI Passport 的代码，需在相应目录内保留其 MIT 许可原文与版权声明。
