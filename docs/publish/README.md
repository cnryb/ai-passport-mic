# 发布物（文案与封面）

这份目录里的东西是**提交给玩法社区的原始素材**，不是给开发者读的文档。
放这里而不是放在仓库外，是因为它们要和固件版本一起被 review：
文案里写的"点哪个按钮"必须与当时的界面一致，改了界面就要回来改这份。

| 文件 | 用途 | 平台限制 |
| --- | --- | --- |
| `description-zh.txt` / `description-en.txt` | 玩法简介（用户挑作品时看的） | 中文 10–4000 字，英文 ≤6000 |
| `instructions-zh.txt` / `instructions-en.txt` | 使用方法（第一次上手照着做） | 各 ≤8000 字；**中文必填** |
| `changelog-zh.txt` / `changelog-en.txt` | 更新日志（仅更新时必填中文） | 各 ≤4000 字 |
| `cover.jpg` | 封面 | **必须严格 3:4**，≤10 MiB |

## 口径硬规则（`docs/PUBLISHING.md` §1 有完整清单）

- **play-first**：只写用户能感知的事。禁用词包括 `ESP32` / `Espressif` / 乐鑫 /
  `ES8311` / `I2C` / `I2S` / `SPI` / `UART` / `GPIO` / `LCD` / `OLED` / `LVGL`、
  屏幕型号与分辨率、Flash 容量、分区、SDK/框架/构建系统。
- **必要准备可以说**（用户必须知道，不算技术规格）：需要 Mac、需要装一个免费的虚拟声卡、
  首次打开要绕过系统安全提示、设备会被覆盖。
- **不许写**：高音质、零延迟、稳定不卡。依据见 `docs/TODO.md` P0（G2 CPU/内存超标、
  KI-003 端到端延迟无数字、KI-006 个别场次仍欠载）。
- 纯文本字段，**不要写 Markdown**（`**粗体**` 会原样显示成星号）。

## 校验

```bash
# 需要官方发布助手（版本会变，重下即可）
curl -sS -o /tmp/publisher.zip https://ai-passport.folotoy.cn/skills/folotoy-ai-passport-publisher.zip
unzip -o /tmp/publisher.zip -d /tmp

python3 /tmp/folotoy-ai-passport-publisher/scripts/publisher.py validate \
  --firmware device/build/ai-passport-mic-merged.bin \
  --cover docs/publish/cover.jpg
```

`validate` 只查格式/大小/比例，**不查图片内容和文案**。文案有没有禁用词、
按钮名对不对，只能靠人（和本目录的 review）保证。

## 封面来源

`cover.jpg` 是**生成的示意图**，不是实机截图。提交时不要在文案里暗示它是真实拍摄。

要求：严格 3:4 竖版（当前 864×1152），画面是玩法场景（设备 + 会议软件），
不是工程图、不是硬件广告图、屏幕区域不能全黑或不可读。
