# desktop/ —— PC 端软件（待落地）

把设备送上来的 BLE 音频解码后写入系统虚拟声卡，让会议软件把它当麦克风用。
**当前目录是空的**：只有这份计划，没有任何代码。

## 计划技术栈

| 层 | 选择 | 理由 |
| --- | --- | --- |
| 语言 | Rust | 与 BLE/音频缓冲的实时性要求匹配，且能出单一可执行文件 |
| 结构 | Cargo workspace | 协议、链路、CLI 分开，协议要能被测试单独覆盖 |
| GUI | Tauri 2 + Vue 3 | 体积小、跨平台；第一版也可以先只做 CLI，GUI 后补 |
| 音频输出 | 写系统虚拟声卡（不自研驱动） | macOS 用 BlackHole，Windows 用 VB-CABLE |

## 计划目录

```text
desktop/
  Cargo.toml          # workspace
  crates/
    protocol/         # 帧、载荷、ADPCM 编解码、按键映射 schema、音质指标
    link/             # BLE 连接与订阅（平台相关代码用 cfg 隔离）
    cli/              # 验收工具：scan / status / record / analyze
  app/                # GUI（可后置；先有 CLI 再套壳）
```

## 落地时必须满足的契约

1. 协议实现必须与 [`../docs/ARCHITECTURE.md`](../docs/ARCHITECTURE.md) 一致
   （帧格式、CRC、UUID、20 ms 块）。
2. 断连、未订阅、虚拟声卡不可用 → 明确状态；**不允许**继续向声卡写静音假装在录音。
3. 序号跳变要计数并上报，不能用"能听到声音"掩盖丢块。
4. 按键注入失败或异常退出时，释放所有按下的修饰键。
5. 不实现烧录、ASR、文字注入——不在第一版范围。

## 构建前置

- Rust stable（落地时用 `desktop/rust-toolchain.toml` 钉住具体版本）。
- 虚拟声卡已安装（见 [`../docs/ENVIRONMENT.md`](../docs/ENVIRONMENT.md)）。
- 参考实现可以读，但不要把别人的整个工程拷进来，见 [`../docs/REFERENCES.md`](../docs/REFERENCES.md)。
