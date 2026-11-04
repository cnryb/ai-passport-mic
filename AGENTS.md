# AGENTS.md —— 仓库规则（AI 协作入口）

本文件是仓库根的唯一入口，只做路由与硬规则。产品是什么、怎么用，看
[`README.md`](README.md)；设计细节看 [`docs/`](docs/)。

## 当前状态（务必先接受这个事实）

本仓库是 **AI Passport Mic 的初始化基线**，不是某个项目的成品拷贝：

- 有：`docs/`（产品/架构/环境/音质/路线图/参考实现）、`config/`（按键映射样例与说明）、
  `scripts/`（环境检查、IDF 激活、初始化校验）。
- 没有：固件代码、桌面端代码。`device/`、`desktop/` 只有 README。
- 因此：不要假设存在 `cargo test`、`idf.py build`、`npm run build` 这类门禁；
  当前唯一的自动检查是 `./scripts/verify.sh`。

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
desktop/  PC 软件（Rust workspace；GUI 走 Tauri 2 + Vue 3）。
config/   按键映射 schema 与样例。
docs/     产品、架构、环境、音质、路线图、参考实现。
scripts/  环境与校验入口。
```

两个子树的规则互不通用：`device/` 的 ESP-IDF/LVGL/烧录约定不适用于 `desktop/`，
反之亦然。子目录一旦建立自己的 `AGENTS.md`，以子树内那份为权威。

## 当前验证入口

```bash
./scripts/check-env.sh          # 只读环境体检（工具链 + ESP-IDF v5.5.3）
source ./scripts/activate-idf.sh # 当前 shell 激活 ESP-IDF
./scripts/verify.sh             # 初始化基线自检：目录、文档内链、脚本权限、环境快照
```

交付时按下面格式报状态，不要合并成一句话：

```text
Structure: PASS / FAIL
Docs links: PASS / FAIL
Env: PASS / FAIL（缺失项列出）
Unverified: 尚未落地的能力（固件、桌面端、真机）
```
