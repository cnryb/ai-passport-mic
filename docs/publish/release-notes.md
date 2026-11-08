## 首次打开 / First launch

这个包是 **ad-hoc 签名、未公证**（没有 Apple 开发者证书，见 [`docs/PUBLISHING.md`](../PUBLISHING.md) §3）。
macOS 会提示「无法打开，因为 Apple 无法检查其是否包含恶意软件」——这不是文件损坏。

解压后**右键点「AI Passport Mic.app」→ 选「打开」→ 再点一次「打开」**。
成功一次之后双击就能启动。也可以在终端执行：

```bash
xattr -dr com.apple.quarantine "/Applications/AI Passport Mic.app"
```

This build is **ad-hoc signed and not notarized** (no Developer ID certificate).
macOS blocks the first launch. Right-click the app, choose Open, then Open again,
or run the `xattr` command above.

## 安装前请读 / Before you start

还得装一个免费的虚拟声卡（BlackHole 2ch），会议软件才能把它当成麦克风。
界面里有下载指引和「验证声卡」一键自检，完整步骤见 [`instructions-zh.txt`](instructions-zh.txt)。

本软件**只支持 macOS**，没有 Windows 版。
