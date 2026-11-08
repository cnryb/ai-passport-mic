# 发布到 FoloToy 玩法社区

目标站点 <https://ai-passport.folotoy.cn/>（中文主站，`/en/` 是英文镜像）。
本文回答三件事：**发布形态是什么、现在还差什么、没有签名证书怎么发**。

> 口径复核日期：2026-11-08。站点与官方发布助手会变，发布前重跑一次
> `publisher.py validate` 与 `tags`，不要凭这份文档里的记忆提交。

---

## 1. 发布形态：是一个"玩法（play）"，不是一个 App

站点是 **玩法社区 + Web 刷机工具**，不是软件下载站：

| 能力 | 路径 | 对交付物的要求 |
| --- | --- | --- |
| 玩法社区 | `/plays/` | 一个作品 = 一份固件 + 一套文案 + 3:4 封面 |
| 安装页 | `/plays/<slug>/install/` | 用户在 Chrome/Edge 里用 WebUSB 直接刷进设备 |
| 刷机工具 | `/tools/web-flasher/` | 支持本地 `.bin`；官方与社区固件都**从 0x0 整片写** |
| 恢复出厂 | 设备背面二维码 → PC 打开 | **装完我们的玩法后，这条路径必须仍然可用**（见 §5） |

平台侧硬门槛（读 `publisher.py` 与创作者中心前端确认）：

- 固件：**从 0x0 可写的合并镜像，≤ 8 MiB**，首字节 `0xE9`，ESP 镜像头合法。
- 封面：**必须有 1 张严格 3:4 竖版** JPG/PNG/WebP，≤ 10 MiB（建议 1152×1536）；
  额外图最多 4 张，也都是 3:4。
- 文案：中文标题（2–80）/ 中文简介（10–4000）/ **中文使用方法必填**（≤ 8000）；
  更新时中文更新日志必填（≤ 4000）。
- 口径：**play-first**。公开展示的文案里不得出现 `ESP32` / `ESP32-C3` / `Espressif` /
  乐鑫 / `ES8311` / `I2C` / `I2S` / `SPI` / `UART` / `GPIO` / `LCD` / `OLED` / `LVGL`、
  屏幕型号与分辨率、Flash 容量、分区、SDK/框架/构建系统。
- 分类与标签：**由服务端 AI 自动判定**，不要作者选、也不要问用户。
- 源码：可选，但必须是**公开 HTTPS 仓库**。
- 流程：提交后是 `pending`（待审核），**不等于已公开**。

## 2. 当前状态

已经满足（有实测证据）：

| 项 | 证据 |
| --- | --- |
| 合并镜像合法、3 段、≤ 8 MiB | `publisher.py validate` 通过（730,736 B / 3 segments） |
| 镜像布局可整片写 | bootloader@0x0、分区表@0x8000、app@0x10000 |
| 应用描述符版本干净 | `version=0.1.0`、`project_name=ai-passport-mic`（无 `-dirty`） |
| 浏览器能识别设备 | 控制台走 USB-Serial-JTAG（`USB JTAG/serial debug unit`） |
| 代码仓库公开 | `github.com/cnryb/ai-passport-mic` |
| 分区表与既有社区作品一致 | 与已上线的 VibeMic 固件同为 `nvs/phy_init/factory(0x7f0000)` |
| 虚拟声卡一键验证可用 | 真机实测 写入 -20.00 dBFS / 回读 -20.24 dBFS / 0 欠载 |

仍缺（按阻塞程度）：

| # | 缺口 | 现状 |
| --- | --- | --- |
| 1 | **对外文案** | 无中英标题/简介/使用方法；仓库内文档全是技术向 |
| 2 | **3:4 封面** | 仓库内唯一视觉资产是 512×512 正方形图标 |
| 3 | **可下载的安装包** | 由 `.github/workflows/macos-build.yml` 在打 `v*` tag 时产出 |
| 4 | **签名与公证** | **不做**（见 §3），产物是 ad-hoc 签名 |
| 5 | **Windows 版** | 只有 macOS arm64 + x86_64；注入层 Windows 未实现 |
| 6 | 公开仓库落后本地提交 | 发布前先 push |

## 3. 无签名证书时的分发口径

**结论：没有 Developer ID 也能发，但用户首次打开要绕过 Gatekeeper，
这一条必须写在文案和 Release 说明里，不能指望用户自己猜。**

### 产物会是什么样

- 签名：`ad-hoc`（`codesign --sign -`），`TeamIdentifier=not set`。
- `spctl -a` 会报 `rejected`，双击提示"无法打开，因为 Apple 无法检查其是否包含恶意软件"。
- 从浏览器下载的 zip 会带 `com.apple.quarantine` 属性，这是被拦的直接原因。

### 用户侧两种打开方式（二选一，都写进文案）

1. **右键打开**：解压后右键点 `AI Passport Mic.app` → 「打开」→ 再点一次「打开」。
   只要成功启动一次，之后双击就行。
2. **命令行去隔离**（适合照文档操作的用户）：

   ```bash
   xattr -dr com.apple.quarantine "/Applications/AI Passport Mic.app"
   ```

### 这条口径带来的三个真实代价（如实写，不粉饰）

1. **审核可能问**。社区现有作品是设备独立可玩的固件，我们这套要配桌面软件。
   审核若因此打回，**不要**把它包装成"不需要 PC"来绕过——那是虚假描述。
   正确做法是在简介里第一句就说清"需要一台 Mac + 一个配套的桌面小工具"。
2. **首次打开的成功率会掉**。这一步没有技术办法绕过，只能把指引写到位。
   所以文案里的打开步骤要放在**使用方法的第 1 条**，不能塞在附录。
3. **`spctl` 失败要在 CI 里显式标注，不能当作 bug**。
   `macos-build.yml` 的打包 job 已经断言"签名必须是 ad-hoc"——将来谁加了证书，
   这里会 FAIL 提醒他去更新文档口径；反过来也不能让 ad-hoc 被误当成"签名坏了"。

### 什么情况下必须补证书

不要为了"体验更好"提前买。满足下面任一条再做：

- 审核明确要求"可直接双击安装"；
- 下载量上来了，首次打开失败成为主要反馈；
- 要做自动更新（Tauri updater 的 macOS 签名校验依赖正式签名）。

Apple Developer Program 是 99 美元/年。补的时候要动的地方：
`tauri.conf.json` 的 `macOS.signingIdentity`、`build-gui.sh` 的签名断言、
`macos-build.yml` 的签名校验分支、本文 §3 全节。

## 4. 发布步骤

### 4.1 准备（一次性）

```bash
# 官方发布助手（skill 包，版本会变，重下即可）
curl -sS -o /tmp/publisher.zip https://ai-passport.folotoy.cn/skills/folotoy-ai-passport-publisher.zip
unzip -o /tmp/publisher.zip -d /tmp

# 校验：固件 + 封面（这一步不需要登录）
python3 /tmp/folotoy-ai-passport-publisher/scripts/publisher.py validate \
  --firmware device/build/ai-passport-mic-merged.bin \
  --cover <封面.png>
```

`validate` 只检查格式、大小、尺寸，**不检查图片内容**。封面必须是
打开看过、确认是成品的图，不能是生成中的占位图。

### 4.2 出固件与安装包

```bash
./scripts/build-firmware.sh      # 合并镜像，落 device/build/
./scripts/build-gui.sh --release # .app，并校验 Info.plist 与随包固件
```

CI 上推 `v*` tag 会自动做同一件事并出双架构 zip + Release：

```bash
git tag v0.1.0        # 必须与 crates/gui/tauri.conf.json 的 version 一致
git push origin v0.1.0
```

### 4.3 提交

在创作者中心（`/account/`，手机号或邮箱登录）「发布新玩法」复制一次性授权码，
或走 AI 发布助手的 `submit --auto`。

注意：

- 一次性授权码 24 小时有效、只能兑换一次；凭据保存在仓库外，**不进版本库**。
- 提交成功是 `pending`，不是已公开。审核通过后才会出现在 `/plays/`。
- 同名作品已存在时不要新建一个来重试，用 `--project-id` 走更新。

## 5. 发布前必须做的一轮真机验证

**这一条不过就不要发。** 分区表虽然与既有社区作品一致，但"装完还能恢复出厂"
这条路径在本项目从未实测过：

1. 用社区页面把我们的固件刷进设备；
2. 扫设备背面二维码 → 打开设备专属页 → 在 PC 上用 Chrome 执行「恢复默认固件」；
3. 确认恢复成功、微信小程序能重新连上。

同时补测（当前都未验）：

- 空片首烧（整片 `0xFF` 裸片）；
- 烧写中途拔线 → 重插恢复；
- 装我们的固件后，设备屏幕显示的文案与实际按键是否一致。

## 6. 文案口径备忘

写简介时**只说用户能感知的事**，不要写实现：

| 不要写 | 改成 |
| --- | --- |
| ESP32-C3 / 8 MB Flash / NimBLE | （删掉，用户不关心） |
| IMA-ADPCM 20 ms 分块 | "开麦几乎立刻出声" |
| 16 kHz 单声道 | "适合说话的清晰度" |
| 虚拟声卡 / BlackHole | "需要先装一个免费的小工具，界面里有下载指引"（首次必须说清，这是必要准备） |

可以如实写进"使用方法"的必要成本（这些不算技术规格，是用户必须知道的准备动作）：
需要一台 Mac、需要装一个免费的虚拟声卡、首次打开要绕过系统安全提示。

**不许**写：高音质、零延迟、稳定不卡。
G2 的 CPU 14.2% / 内存 208 MB 仍超标（见 `TODO.md` P0），
端到端声学延迟仍无数字（KI-003），KI-006 个别场次仍会短暂欠载。

## 7. 更新已发布作品

- `projects[].publishedRevision` 是上一次公开版本，用它对比实际改动。
- 更新日志**只写本次变化**，不复制历史日志，也不写未来计划。
- 使用方法若有变化要同步修订；没变就沿用。
- 提交的是**完整版本**（含固件与 3:4 封面），不是增量补丁。
- 有草稿或待审核版本时停止并报告冲突，不要覆盖或撤回。
