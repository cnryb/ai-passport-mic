# 按键映射配置

`button-map.example.json` 是**可执行样例**（第一版还没有代码消费它，但字段已经定死，
实现时按这份说明来，不要静默改名）。

```json
{
  "schema": "ai-passport-mic-keymap-1",
  "bindings": {
    "up.short":    { "modifiers": ["ctrl"], "key": "enter" },
    "ok.press":    { "modifiers": [],       "key": "ptt-start" },
    "ok.release":  { "modifiers": [],       "key": "ptt-stop" }
  }
}
```

## 字段

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `schema` | string | 固定 `ai-passport-mic-keymap-1`。版本号变了就要写迁移说明 |
| `bindings` | object | 键是**事件名**，值是动作对象；未出现的事件使用默认值 |
| `bindings.*.modifiers` | string[] | `ctrl` / `shift` / `alt` / `win`（macOS 上 `win` 即 Command），空数组表示无修饰键 |
| `bindings.*.key` | string | 目标键名（小写，如 `enter` / `esc` / `a` / `f5`），或保留动作 `ptt-start` / `ptt-stop` |
| `bindings.*.repeat` | number | 可选，目标键**连发次数**，缺省 1，上限 4。例：`down.double` + `repeat: 2` = 双击时连发两记 Esc |

## 事件名

`up` / `down` / `ok` × `short`（短按）/ `double`（双击）/ `long`（长按）/ `press`（按下）/
`release`（抬起）。例如 `up.short`、`ok.press`、`ok.release`。

两件事要分清：**事件名描述手势**（`double` = 用户双击了一次），**`repeat` 描述动作**
（把这个键连发几次）。不要用 `count` 之类的名字表达手势次数——那会和事件名重复，
产生 "double + count:3" 这种说不清的组合。要支持三连击就新增事件名 `triple`，
并同步升级 `schema` 版本。

## 硬约束

1. **`ok.press` / `ok.release` 是 PTT 开关**，默认必须保留。若允许用户改写，必须同时提供
   一个可靠的"停止采集"通道，否则会出现麦克风停不下来的情况。
2. 注入快捷键时，无论成功或异常，都要释放所有按下的修饰键（Ctrl/Shift/Alt/Win）。
3. 双击窗口按 **180 ms** 处理（第二下按下落在第一下抬起后的窗口内）；窗口内不再补发单击。
4. 导入时的校验顺序：`schema` 版本 → 事件名合法 → 键名/修饰键在支持列表内 → PTT 约束。
   任一步失败整份拒绝，不做"部分生效"。
5. 导出只写当前生效的绑定，不写注释与未知字段，保证 round-trip 稳定。

## 与固件的关系

按键的**物理判定**（消抖、双击窗口、长按阈值）在固件；**映射到哪个键**在 PC 端。
两边都要能独立测试，联调时用固定的按键事件序列做回归。
