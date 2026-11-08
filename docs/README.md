# docs/ —— 文档索引

本目录是**唯一的设计事实来源**。根 [`../README.md`](../README.md) 只讲"这是什么、
怎么跑起来"；协作规矩在 [`../AGENTS.md`](../AGENTS.md)。

> 官方玩法开发提示词要求"先读 `AGENTS.md` 和 `docs/README.md`"，
> 这份索引就是给人和 AI 一个稳定的入口。**新增文档必须在这里登记**，
> 否则下一个人只能靠 `ls` 猜。

## 先读哪一份

| 你的目的 | 读这份 |
| --- | --- |
| 我要改协议（帧格式、UUID、块长、延迟预算） | [`ARCHITECTURE.md`](ARCHITECTURE.md) —— **先改文档再改代码** |
| 我要知道"现在做到哪一步、还有什么没做" | [`ROADMAP.md`](ROADMAP.md)（里程碑结论）+ [`TODO.md`](TODO.md)（未闭环项） |
| 我要查"这个问题是不是已知的" | [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md) |
| 我要碰界面 | [`GUI.md`](GUI.md)（方案与决定）+ [`GUI_IMPLEMENTATION_LOG.md`](GUI_IMPLEMENTATION_LOG.md)（每轮实测过程） |
| 我要判断"音质算不算合格" | [`SOUND_QUALITY.md`](SOUND_QUALITY.md) |
| 我要搭环境 | [`ENVIRONMENT.md`](ENVIRONMENT.md) |
| 我要发布到玩法社区 | [`PUBLISHING.md`](PUBLISHING.md) |
| 我要知道能参考哪些外部实现 | [`REFERENCES.md`](REFERENCES.md) |

## 各文档写什么、不写什么

### [`PRODUCT.md`](PRODUCT.md) —— 产品定义
一句话目标、用户场景、第一版边界（做什么/明确不做什么）、开箱体验的硬约束。
**范围之争以这份为准**：要加功能先改这里。

不写：实现细节、里程碑排期。

### [`ARCHITECTURE.md`](ARCHITECTURE.md) —— 跨端契约
协议帧与 CRC、ADPCM 分块、BLE 服务 UUID、连接参数两档、延迟预算、功耗取舍、
分发与安装成本（含 BlackHole 的 GPL-3.0 约束）。

**这是跨端契约，代码必须与它一致。** 改协议先改这里，两端同步改，
并让 `device/test/test_protocol.c` 覆盖到。

不写：某次调试的过程与失败方案。

### [`ROADMAP.md`](ROADMAP.md) —— 里程碑与结论
M0–M6 与 G0–G6 每期做到什么、判据是什么、**实测结论**是什么（含 FAIL 的）。

已完成的里程碑写结论与证据出处；未完成的不在这里列待办（去 `TODO.md`）。

### [`TODO.md`](TODO.md) —— 还没闭环的事
按 P0（真机闭环）→ P4（文档债）分组，每条带标记：
`[验收]` 需真机/人在场 · `[功能]` 未实现 · `[根因]` 已缓解未根治 · `[债]` 技术/文档债。

**已完成的事不写在这里**；只写"下一步要做什么、缺什么证据"。

### [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md) —— 已知问题与证据链
KI-001…KI-009。每条含：现象、复现方式、已排除的假设、当前缓解措施、
**尚未验证的部分**。判据与阈值的合理性争议也记在这里。

**不要在这里写"已解决"除非有真机证据**；已关闭的排查路径要写明"别再查一遍"。

### [`GUI.md`](GUI.md) + [`GUI_IMPLEMENTATION_LOG.md`](GUI_IMPLEMENTATION_LOG.md)
前者是方案与决定（D1 编排只有一份、§7 指标口径、§10 验收表、§14 门禁明细）；
后者是**每一轮实际做了什么、测出了什么数字**（§14.x 按轮次编号）。

两份分开的理由：方案要能被人读懂，实施记录要能被人查证。混在一起
就变成"方案里塞满了历史踩坑，谁也读不出当前设计是什么"。

### [`SOUND_QUALITY.md`](SOUND_QUALITY.md) —— 音质判据
客观指标阈值（RMS、峰值、削波比例、直流偏置）、固定场景矩阵、
端到端延迟目标、以及**哪些结论还不能下**。

### [`ENVIRONMENT.md`](ENVIRONMENT.md) —— 宿主环境
实测过的机器与工具链版本、ESP-IDF 安装坑（python venv 与小版本绑定）、
功耗基线的测量口径（表**当前是空的**，填之前不许写省电结论）。

### [`PUBLISHING.md`](PUBLISHING.md) —— 发布到 FoloToy 玩法社区
发布形态（Web 刷机、3:4 封面、play-first 文案口径）、当前缺口、
**无签名证书时的分发口径与用户侧首次打开步骤**。

### [`REFERENCES.md`](REFERENCES.md) —— 外部参考
只讲"可以借什么、不可以借什么"，含许可证。**不 vendor、不 fork、不整目录拷贝**。

## 维护规则

1. **新增文档要在这里登记**，写清"写什么、不写什么"。
2. 文档里的**实测数字**随代码变，只靠人回填。数字变了就改，别留旧值
   （已知痛点见 `TODO.md` 的 P4「实测数字没有统一更新机制」）。
3. 文档之间**不重复同一张表**。重复即漂移来源：验证状态收敛到
   `ROADMAP.md` 与 `GUI.md` §10，`TODO.md` 只引用不抄。
4. 引用上游结论时写明来源（项目 + 文件 + 大致位置），见 `REFERENCES.md` 的引用规范。
