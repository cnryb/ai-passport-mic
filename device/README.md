# device/ —— 固件（待落地）

ESP32-C3 上的采集与 BLE 传输固件。**当前目录是空的**：只有这份计划，没有任何代码，
也没有上游 fork 的残留。

## 计划技术栈

| 项 | 值 |
| --- | --- |
| 框架 | ESP-IDF **v5.5.3**（固定版本） |
| 语言 | C（遵循 IDF 组件化写法） |
| 目标 | ESP32-C3，8 MB Flash，无 PSRAM |
| BLE | NimBLE，单连接 peripheral：HID 键盘 + 自定义 GATT 服务 |
| 控制台 | USB-Serial-JTAG（C3 原生 USB），避免占用 UART0 引脚 |

## 计划目录

```text
device/
  CMakeLists.txt
  partitions.csv          # nvs + phy_init + factory
  sdkconfig.defaults      # 目标芯片、Flash、NimBLE、LVGL 等基线配置
  main/
    app.c                 # 应用入口与状态机
    audio.c               # ES8311 采集 + IMA-ADPCM 编码
    link.c                # GATT 服务与通知
    buttons.c             # 按键消抖、单击/双击/长按判定
    protocol.c            # 帧编解码（与 PC 端同一组测试向量）
  components/
    bsp/                  # 板级：引脚、I2S、显示、电源
```

## 落地顺序（对应路线图 M1）

1. 工程骨架 + 分区表 + 控制台能打印。
2. ES8311 采集出声（先落 WAV 到串口/内存，不做 BLE）。
3. ADPCM 编码 + 序号，与 PC 端测试向量对齐。
4. GATT 服务与通知，MTU ≥ 168 才能单包发 165 字节音频块。
5. 按键事件：本地 HID + GATT 事件双路径。

## 硬约束

1. **不要整份 fork 上游仓库。** 需要引脚定义、Codec 初始化、BLE HID 描述符这类
   基础设施时，按本项目结构重写，并在文件头注明来源与 MIT 许可（见
   [`../docs/REFERENCES.md`](../docs/REFERENCES.md)）。
2. 协议改动先改 [`../docs/ARCHITECTURE.md`](../docs/ARCHITECTURE.md)，两端同步。
3. LVGL 不是线程安全的；非 LVGL 任务访问 UI 对象必须加锁。
4. 按键回调里不许做阻塞操作（音频、存储、网络都放到 worker task）。
5. 烧录会改动设备上的数据（绑定、配置）；动手前先跟用户确认，不要默认执行。
6. 中文 UI 文本要单独确认字体覆盖，UTF-8 + 编译通过不等于能显示。
