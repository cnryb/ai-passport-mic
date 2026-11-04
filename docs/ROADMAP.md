# 路线图

## M0：项目初始化（已完成）

- [x] 确定产品边界：纯无线麦克风，不做 ASR / 文字注入 / AI Agent。
- [x] 仓库骨架：`device/`、`desktop/`、`config/`、`docs/`、`scripts/`。
- [x] 文档：产品、架构（含协议契约）、环境、音质评估方法、参考实现。
- [x] 按键映射 schema 与可执行样例（`config/`）。
- [x] 环境脚本：`check-env.sh`（只读体检）、`activate-idf.sh`、`verify.sh`（基线自检）。

> M0 的定义是「仓库能交给下一个人接手」，不是「软件能用」。代码尚未开始，属正常状态。

## M1：固件最小链路（自己实现）

- ESP-IDF 工程骨架 + 8 MB 分区表 + USB-Serial-JTAG 控制台。
- ES8311 采集：16 kHz / 16-bit / mono。
- IMA-ADPCM 编码，20 ms 一块（320 samples → 160 字节 + 5 字节块头）。
- BLE 自定义 GATT：CTRL / PROFILE / STATUS / EVENT / AUDIO 五个特征值。
- 按键（UP/DOWN/OK）事件：本地 HID + GATT 事件双路径。

## M2：桌面端最小链路

- Rust 设备链路：BLE 连接、订阅、序号检测、ADPCM 解码。
- 输出到虚拟声卡（BlackHole / VB-CABLE），带抖动缓冲。
- 断连 / 未订阅 / 声卡不可用必须有明确状态，不能静默假装在录音。
- `apmctl record` 输出 WAV，作为链路验收工具。

## M3：按键自定义

- 编辑器支持 UP / DOWN / OK 的短按、双击、长按、按下、抬起。
- 配置持久化、恢复默认、导入/导出（schema 见 `config/README.md`）。
- 保证 OK 的 PTT 停止路径不会被配置破坏。
- 快捷键注入无论成功或异常，都要释放所有按下的修饰键。

## M4：音质验收

- `apmctl analyze` 输出完整 JSON 报告（字段见 `docs/SOUND_QUALITY.md`）。
- 增加启动延迟与稳态端到端延迟测量。
- 完成固定场景矩阵，建立版本对比基线。
- 评估是否需要重采样、AGC、降噪或更高码率编码。

## M5：产品化

- 安装包、签名、自动更新、崩溃日志脱敏。
- 虚拟声卡安装引导与故障诊断。
- 长时间稳定性、功耗与热测试。
- 再评估标准蓝牙 HFP 或原生驱动是否可行。

## 已知约束

- 只做 BLE 自定义 GATT：标准 BLE HFP 在 macOS/Windows 上角色、编码协商和系统音频路由
  差异太大，不适合作为第一步；代价是用户必须装虚拟声卡。
- 设备端无 PSRAM，音频缓冲与 UI 内存都要按 C3 的余量规划。
- 参考实现可以看，但不能整份搬进来（见 `docs/REFERENCES.md`）。
