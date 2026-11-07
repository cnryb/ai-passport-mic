<script setup lang="ts">
/**
 * 按键映射（G5）：按**角色**分三块，而不是按事件名平铺。
 *
 * - 麦克风：开麦键（三键里选一个，绑 `ptt-toggle`）+ 开麦方式（hold 未实现，占位）
 * - 快捷键映射：除开麦键之外的按键分别注入什么
 * - 采集时注入：开麦/关麦那一刻自动点按的快捷键（`hooks`）
 *
 * 分区只是展示：落盘仍是同一个 `keymap-4` 文件（`bindings` + `hooks`）。
 * 校验全在 Rust 侧（`apm_app::keymap_store` → `apm_protocol::keymap`），前端只收集输入。
 * PTT 约束是「整个映射**恰好一个**键绑 `ptt-toggle`」，不是「`ok` 必须是」。
 *
 * 注入（M3）已实现：开麦/关麦钩子真的会往前台应用敲键。前提是**辅助功能权限**，
 * 没权限时系统静默丢弃按键——所以缺权限时页面顶部有引导横幅（每 2 秒复查）。
 *
 * ## 版面（2026-11-07 重做）
 * 工具栏压成一行（路径可省略），权限横幅瘦身成图标 + 三步 + 右侧两个按钮，
 * 每块的说明文字移进卡内底部。旧版四段长说明平铺在页面底部，与控件混在一起，
 * 加上横幅占位 190px，最小窗口下"采集时注入"整个区块被挤出可视区。
 */
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { EVENT_LABEL, KEY_CHOICES, type BindingInput, type KeymapView } from '../lib/types'
import { api } from '../lib/ipc'

const props = defineProps<{
  view: KeymapView | null
  busy: boolean
  error: string | null
}>()

const emit = defineEmits<{
  (e: 'save', bindings: BindingInput[]): void
  (e: 'reset'): void
  (e: 'import', text: string): void
  (e: 'copy'): void
}>()

const MODS = ['ctrl', 'shift', 'alt', 'win'] as const
/** 设备上的物理按键，顺序即下拉顺序。开麦键只能从这里选一个。 */
const KEY_EVENTS = ['up', 'down', 'ok'] as const
/** 开麦动作的保留键名，界面不给用户填，落盘时统一写这个。 */
const PTT_TOGGLE = 'ptt-toggle'

const rows = ref<BindingInput[]>([])
/** 当前的开麦键（绑 `ptt-toggle` 的事件名）。 */
const pttEvent = ref<string>('ok')
/** 换开麦键时被清掉的旧绑定的提示（换完就说明白了，保存与否不影响它成立）。 */
const pttNotice = ref<string | null>(null)
const importText = ref('')
const showImport = ref(false)

/** 快捷键块：除开麦键之外的物理按键。 */
const shortcutRows = computed(() =>
  rows.value.filter(
    (r) => KEY_EVENTS.includes(r.event as (typeof KEY_EVENTS)[number]) && r.event !== pttEvent.value,
  ),
)
/** 采集钩子块：`capture.start` / `capture.stop`。 */
const hookRows = computed(() =>
  rows.value.filter((r) => !KEY_EVENTS.includes(r.event as (typeof KEY_EVENTS)[number])),
)

// 辅助功能权限：null = 还没查到（避免闪横幅）；false = 缺权限，显示引导；
// true = 已就绪，横幅收起。缺权限时轮询复查，用户勾完回来不用手动刷新。
const injectOk = ref<boolean | null>(null)
let pollTimer: ReturnType<typeof setInterval> | null = null

async function checkInject() {
  try {
    injectOk.value = await api.injectPermission()
  } catch {
    injectOk.value = null
  }
  if (injectOk.value === false) startPolling()
  else stopPolling()
}

function startPolling() {
  if (pollTimer) return
  pollTimer = setInterval(checkInject, 2_000)
}

function stopPolling() {
  if (pollTimer) {
    clearInterval(pollTimer)
    pollTimer = null
  }
}

async function openAccessibilitySettings() {
  try {
    await api.openAccessibilitySettings()
  } catch {
    // 打不开就退回手动路径，横幅里的文字本身就是指引
  }
}

onMounted(checkInject)
onUnmounted(stopPolling)

watch(
  () => props.view,
  (v) => {
    if (!v) return
    rows.value = v.bindings.map((b) => ({
      event: b.event,
      modifiers: [...b.modifiers],
      key: b.key,
      repeat: b.repeat,
    }))
    pttEvent.value = v.ptt_event ?? 'ok'
    pttNotice.value = null
  },
  { immediate: true },
)

function toggleMod(row: BindingInput, mod: string) {
  const at = row.modifiers.indexOf(mod)
  if (at >= 0) row.modifiers.splice(at, 1)
  else row.modifiers.push(mod)
}

/**
 * 换开麦键。有两条对称的语义代价，都在换的瞬间生效并当场提示：
 *
 * 1. **旧开麦键变回「未绑定 = 不动作」**，不接手任何注入键——让它继承会变成
 *    "习惯性按 OK 的人突然开始往前台应用敲键"，比什么都不做危险。
 * 2. **被选为开麦键的那个键，它原来的快捷键也消失**（那个键不再是快捷键，
 *    没有"按哪个键"可填）。这条更隐蔽，所以必须显式提示，不能静默丢。
 */
function changePtt(next: string) {
  if (next === pttEvent.value) return
  const lost: string[] = []

  const old = rows.value.find((r) => r.event === pttEvent.value)
  if (old && old.key.trim() && old.key !== PTT_TOGGLE) {
    lost.push(`${label(old.event)} 原来的 ${describe(old)}`)
  }
  if (old) {
    old.key = ''
    old.modifiers = []
    old.repeat = 1
  }

  const promoted = rows.value.find((r) => r.event === next)
  if (promoted && promoted.key.trim()) {
    lost.push(`${label(next)} 原来的 ${describe(promoted)}`)
    promoted.key = ''
    promoted.modifiers = []
    promoted.repeat = 1
  }

  pttEvent.value = next
  pttNotice.value = lost.length ? `${lost.join('；')}已清空，保存后生效` : null
}

function label(event: string): string {
  return EVENT_LABEL[event] ?? event
}

function describe(row: BindingInput): string {
  if (!row.key.trim()) return '（空）'
  const mods = row.modifiers.length ? `${row.modifiers.join('+')}+` : ''
  return `${mods}${row.key}${row.repeat > 1 ? ` ×${row.repeat}` : ''}`
}

function save() {
  emit(
    'save',
    rows.value.map((r) => {
      // 开麦键在落盘时统一成 `ptt-toggle`：界面上不给它填键名，也没有连发概念。
      if (r.event === pttEvent.value) {
        return { event: r.event, modifiers: [], key: 'ptt-toggle', repeat: 1 }
      }
      return {
        event: r.event,
        modifiers: [...r.modifiers],
        key: r.key.trim().toLowerCase(),
        repeat: r.repeat,
      }
    }),
  )
}

function doImport() {
  emit('import', importText.value)
  showImport.value = false
}
</script>

<template>
  <div class="keymap">
    <div class="toolbar">
      <span class="path mono" :title="view?.path ?? ''">{{ view?.path ?? '…' }}</span>
      <button class="sm" @click="showImport = !showImport">{{ showImport ? '收起导入' : '导入 JSON' }}</button>
      <button class="sm" :disabled="busy" @click="emit('copy')">导出</button>
      <button class="sm ghost" :disabled="busy" @click="emit('reset')">恢复默认</button>
      <button class="sm primary" :disabled="busy" @click="save">保存</button>
    </div>

    <div v-if="showImport" class="import">
      <textarea
        v-model="importText"
        rows="5"
        placeholder="粘贴 keymap JSON（schema: ai-passport-mic-keymap-4；v3 也能导入，会补上空 hooks 升到 v4）"
      />
      <button class="sm" :disabled="!importText.trim()" @click="doImport">导入并校验</button>
    </div>

    <div v-if="injectOk === false" class="perm" role="alert">
      <span class="perm-ico">!</span>
      <div class="perm-body">
        <b>注入还没生效：缺少辅助功能权限</b>
        <ol>
          <li>点右边按钮 → 系统弹窗里选<strong>打开系统设置</strong></li>
          <li>在辅助功能列表里打开 <strong>AI Passport Mic</strong> 的开关（列表里原先没有它是正常的）</li>
          <li>回到本页，横幅会自动消失，不用重启 App</li>
        </ol>
        <p class="note">
          没有这个权限时系统会<strong>静默丢弃</strong>注入的按键：开麦/关麦看起来正常，
          但目标应用什么都没收到，也不会有任何报错。
        </p>
      </div>
      <div class="perm-act">
        <button class="sm primary" @click="openAccessibilitySettings">打开系统设置</button>
        <button class="sm ghost" @click="checkInject">重新检查</button>
      </div>
    </div>
    <p v-else-if="injectOk === true" class="ok-line">辅助功能权限已就绪，注入可用。</p>

    <p v-if="error" class="err">{{ error }}</p>
    <p v-if="view?.error" class="err">{{ view.error }}</p>

    <div class="sections scroll">
      <!-- 1. 麦克风 -->
      <section class="sec">
        <header class="sec-head">
          <h3>麦克风</h3>
          <span>按一下开麦，再按一下关麦</span>
        </header>
        <div class="sec-body">
          <div class="row grid-mic">
            <span class="name">开麦键</span>
            <select
              :value="pttEvent"
              aria-label="开麦键"
              @change="changePtt(($event.target as HTMLSelectElement).value)"
            >
              <option v-for="k in KEY_EVENTS" :key="k" :value="k">{{ EVENT_LABEL[k] ?? k }}</option>
            </select>
            <span class="note inline-note">三键里只能有一个开麦键</span>
          </div>
          <div class="row grid-mic">
            <span class="name">开麦方式</span>
            <select disabled aria-label="开麦方式">
              <option>按一下开、再按一下关</option>
              <option disabled>按住说话（未实现）</option>
            </select>
            <span class="note inline-note">「按住不放」还没做，缺 PC 端消费抬起事件</span>
          </div>
          <p v-if="pttNotice" class="notice" role="status">{{ pttNotice }}</p>
          <p class="hint">
            换开麦键后，<strong>原来那个键会变回「不动作」</strong>；被选中的键如果原本有快捷键，
            那个快捷键也会被清掉（同一个键不能既开关采集又注入按键）。
            开麦/关麦是同一个键的开关；忘记关麦有自动停止兜底（见「实时」页设备展开区）。
          </p>
        </div>
      </section>

      <!-- 2. 快捷键映射 -->
      <section class="sec">
        <header class="sec-head">
          <h3>快捷键映射</h3>
          <span>按下这个键，向前台应用敲一下</span>
        </header>
        <div class="sec-body">
          <div v-for="row in shortcutRows" :key="row.event" class="row grid-map">
            <span class="name">{{ EVENT_LABEL[row.event] ?? row.event }}</span>
            <div class="mods">
              <button
                v-for="m in MODS"
                :key="m"
                class="chip"
                :class="{ on: row.modifiers.includes(m) }"
                @click="toggleMod(row, m)"
              >
                {{ m }}
              </button>
            </div>
            <input
              v-model="row.key"
              class="key"
              list="key-choices"
              placeholder="留空 = 不动作"
              spellcheck="false"
            />
            <select v-model.number="row.repeat" aria-label="连发次数">
              <option :value="1">×1</option>
              <option :value="2">×2</option>
              <option :value="3">×3</option>
              <option :value="4">×4</option>
            </select>
          </div>
          <p class="hint">
            注入是<strong>点按</strong>（按下 + 弹起），不是按住不放。
            键名 <code>ralt</code> / <code>lalt</code> 是单独点按右 / 左 Alt（macOS 上即 Option），
            适合「单按右 Alt」这类热键。
          </p>
        </div>
      </section>

      <!-- 3. 采集时注入 -->
      <section class="sec">
        <header class="sec-head">
          <h3>采集时注入</h3>
          <span>开麦 / 关麦那一刻自动敲一下</span>
        </header>
        <div class="sec-body">
          <div v-for="row in hookRows" :key="row.event" class="row grid-hook">
            <span class="name">{{ EVENT_LABEL[row.event] ?? row.event }}</span>
            <div class="mods">
              <button
                v-for="m in MODS"
                :key="m"
                class="chip"
                :class="{ on: row.modifiers.includes(m) }"
                @click="toggleMod(row, m)"
              >
                {{ m }}
              </button>
            </div>
            <input
              v-model="row.key"
              class="key"
              list="key-choices"
              placeholder="留空 = 不动作"
              spellcheck="false"
            />
            <select v-model.number="row.repeat" aria-label="连发次数">
              <option :value="1">×1</option>
              <option :value="2">×2</option>
              <option :value="3">×3</option>
              <option :value="4">×4</option>
            </select>
          </div>
          <p class="hint">
            关麦这一行<strong>也包括链路断开导致的自动停止</strong>。
            这里不能填开麦动作（会在开麦瞬间把自己关掉），后端会整份拒绝。
          </p>
        </div>
      </section>
    </div>

    <datalist id="key-choices">
      <option v-for="k in KEY_CHOICES" :key="k" :value="k" />
    </datalist>
  </div>
</template>

<style scoped>
.keymap {
  display: flex;
  flex-direction: column;
  gap: var(--s2);
  min-height: 0;
  flex: 1 1 auto;
}

.toolbar {
  display: flex;
  align-items: center;
  gap: 6px;
  flex-wrap: wrap;
}

.path {
  flex: 1 1 auto;
  min-width: 0;
  font-size: var(--fs-10);
  color: var(--dim);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.import {
  display: flex;
  flex-direction: column;
  gap: 6px;
  align-items: flex-start;
}

.import textarea {
  width: 100%;
  font: var(--fs-11) / 1.6 var(--mono);
  resize: vertical;
}

/* ---------- 权限横幅：图标 + 三步 + 右侧按钮，横排不再纵向堆 190px ---------- */
.perm {
  display: flex;
  align-items: flex-start;
  gap: 10px;
  border: 1px solid #ffb02038;
  background: #ffb0200d;
  border-radius: var(--r-md);
  padding: 10px 12px;
}

.perm-ico {
  width: 16px;
  height: 16px;
  flex: 0 0 auto;
  margin-top: 1px;
  border-radius: 50%;
  border: 1.5px solid var(--warn);
  color: var(--warn);
  font-size: var(--fs-10);
  font-weight: 700;
  display: flex;
  align-items: center;
  justify-content: center;
}

.perm-body {
  flex: 1 1 auto;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 5px;
}

.perm-body b {
  font-size: var(--fs-12);
  color: #f0c48c;
  font-weight: 600;
}

.perm-body ol {
  margin: 0;
  padding-left: 16px;
  font-size: var(--fs-11);
  line-height: 1.7;
  color: var(--muted);
}

.perm-body ol strong {
  color: var(--text);
  font-weight: 500;
}

.perm-act {
  display: flex;
  gap: 6px;
  flex: 0 0 auto;
  flex-wrap: wrap;
  justify-content: flex-end;
}

.ok-line {
  margin: 0;
  font-size: var(--fs-11);
  color: #7ecf8e;
}

.err {
  margin: 0;
  color: var(--err);
  font-size: var(--fs-12);
  user-select: text;
}

/* ---------- 三块 ---------- */
.sections {
  flex: 1 1 auto;
  min-height: 0;
  overflow-y: auto;
  display: flex;
  flex-direction: column;
  gap: var(--gap-card);
}

.sec {
  border: 1px solid var(--line-soft);
  border-radius: var(--r-md);
  background: var(--panel);
  overflow: hidden;
}

.sec-head {
  display: flex;
  align-items: baseline;
  gap: var(--s2);
  padding: 8px 12px;
  border-bottom: 1px solid var(--line-soft);
  background: #ffffff02;
}

.sec-head span {
  font-size: var(--fs-11);
  color: var(--dim);
}

.sec-body {
  padding: var(--s2) 12px 10px;
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.row {
  display: grid;
  align-items: center;
  gap: 10px;
  padding: 5px 6px;
  border-radius: var(--r-sm);
}

.row:hover {
  background: #ffffff04;
}

.grid-mic {
  grid-template-columns: 64px 150px minmax(0, 1fr);
}

.grid-map {
  grid-template-columns: 76px auto minmax(90px, 1fr) 56px;
}

.grid-hook {
  grid-template-columns: 108px auto minmax(90px, 1fr) 56px;
}

.name {
  font-size: var(--fs-12);
}

.inline-note {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.mods {
  display: flex;
  gap: var(--s1);
}

.chip {
  padding: 2px 8px;
  font-size: var(--fs-10);
  border-radius: var(--r-pill);
  background: transparent;
  color: var(--dim);
  border: 1px solid var(--line);
  min-width: 0;
}

.chip:hover:not(:disabled) {
  background: #ffffff0a;
  border-color: #33404f;
}

.chip.on {
  background: #4c8dff20;
  border-color: #4c8dff66;
  color: #bcd5ff;
}

.key,
select {
  font-family: var(--mono);
  font-size: var(--fs-11);
  width: 100%;
  min-width: 0;
  padding: 4px 7px;
}

select:disabled {
  opacity: 0.55;
}

.hint {
  margin: 7px 6px 0;
  font-size: var(--fs-10);
  line-height: 1.7;
  color: var(--dim);
}

.hint strong {
  color: var(--muted);
  font-weight: 500;
}

.hint code {
  background: #ffffff0a;
  padding: 0 3px;
  border-radius: 3px;
  color: var(--muted);
}

.notice {
  margin: 6px 6px 0;
  padding: 5px 8px;
  font-size: var(--fs-11);
  line-height: 1.55;
  color: #f0c48c;
  background: #ffb0200d;
  border: 1px solid #ffb02038;
  border-radius: var(--r-sm);
}

.note {
  font-size: var(--fs-10);
  line-height: 1.65;
  color: var(--dim);
  margin: 0;
}

.note strong {
  color: var(--muted);
  font-weight: 500;
}
</style>
