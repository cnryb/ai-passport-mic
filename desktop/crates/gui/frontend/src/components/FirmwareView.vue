<script setup lang="ts">
/**
 * 开箱页（M6）：识别设备 → 比对版本 → 写入固件 → 校验。
 *
 * 口径见 `docs/PRODUCT.md` 的「开箱体验」，实现见 `apm_app::firmware`：
 * - 烧写会让设备**复位**进下载模式，正在推流时别点；
 * - 不擦 NVS：配对与按键映射保留，重新烧写不等于恢复出厂；
 * - 校验不通过就不显示"完成"——这里不会把失败包装成"完成，但有警告"。
 *
 * ## 版面（2026-11-07 重做）
 * 版本号原先出现三遍（顶栏 / 卡头 / 卡内格），现在只在卡头出现一次：它是这张卡的主语。
 * 长路径改成「按钮 + 省略号 + tooltip」，行为说明折叠进 details——这页是低频操作，
 * 不该用四段说明占满整屏。
 */
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { api, asAppError, onFirmwareProgress } from '../lib/ipc'
import {
  ERROR_HINTS,
  type AppError,
  type FirmwareStatus,
  type FlashReport,
  type ProbeResult,
} from '../lib/types'

const status = ref<FirmwareStatus | null>(null)
const probes = ref<Record<string, ProbeResult>>({})
const busy = ref<{ port: string; kind: 'probe' | 'flash' } | null>(null)
const progress = ref(0)
const report = ref<FlashReport | null>(null)
const error = ref<AppError | null>(null)

let unlisten: (() => void) | undefined

const errorHint = computed(() => (error.value ? ERROR_HINTS[error.value.code] : ''))
const canFlash = computed(() => status.value?.image != null && busy.value === null)

async function refresh() {
  try {
    status.value = await api.firmwareStatus()
  } catch (e) {
    error.value = asAppError(e)
  }
}

function stateLabel(port: string): string {
  const p = probes.value[port]
  if (!p) return '未检测'
  switch (p.state) {
    case 'up_to_date':
      return `已是最新 ${p.image_version}`
    case 'blank':
      return '空片，需要首烧'
    case 'mismatch':
      return `设备 ${p.current_version ?? '未知'} → 包内 ${p.image_version}`
    default:
      return '版本未知（可能是旧固件或描述符损坏）'
  }
}

function stateTone(port: string): 'dim' | 'ok' | 'warn' {
  const p = probes.value[port]
  if (!p) return 'dim'
  if (p.state === 'up_to_date') return 'ok'
  return 'warn'
}

async function probe(port: string) {
  if (busy.value) return
  busy.value = { port, kind: 'probe' }
  error.value = null
  try {
    probes.value = { ...probes.value, [port]: await api.firmwareProbe(port) }
  } catch (e) {
    error.value = asAppError(e)
    const { [port]: _drop, ...rest } = probes.value
    probes.value = rest
  } finally {
    busy.value = null
  }
}

async function flash(port: string) {
  if (!canFlash.value) return
  busy.value = { port, kind: 'flash' }
  error.value = null
  report.value = null
  progress.value = 0
  try {
    const done = await api.firmwareFlash(port)
    report.value = done
    // 校验通过 = flash 内容与镜像逐字节一致，版本必然等于镜像版本。
    // 这是推断，不是重新探测——想复核再点一次「检测」。
    probes.value = {
      ...probes.value,
      [port]: {
        port,
        chip: probes.value[port]?.chip ?? 'ESP32-C3',
        current_version: done.to_version,
        image_version: done.to_version,
        state: 'up_to_date',
        first_flash: false,
      },
    }
  } catch (e) {
    error.value = asAppError(e)
  } finally {
    busy.value = null
    progress.value = 0
  }
}

async function revealImage() {
  const path = status.value?.image?.path
  if (!path) return
  try {
    await api.revealPath(path)
  } catch (e) {
    error.value = asAppError(e)
  }
}

onMounted(async () => {
  await refresh()
  unlisten = await onFirmwareProgress((percent) => {
    progress.value = percent
  })
})

onUnmounted(() => unlisten?.())
</script>

<template>
  <div class="fw">
    <section class="card">
      <div class="card-head">
        <h2>随包固件</h2>
        <span class="spacer" />
        <span v-if="status?.image" class="tag">{{ status.image.version }}</span>
      </div>

      <template v-if="status?.image">
        <div class="grid">
          <div class="cell">
            <span>大小</span><b>{{ Math.round(status.image.bytes / 1024) }} KB</b>
          </div>
          <div class="cell">
            <span>app 分区 MD5</span>
            <b class="mono" :title="status.image.md5">{{ status.image.md5.slice(0, 16) }}…</b>
          </div>
          <div class="cell">
            <span>写入范围</span><b>3 段 · 保留 NVS</b>
          </div>
        </div>
        <div class="path-row">
          <button class="sm ghost" @click="revealImage">在访达中显示</button>
          <span class="path mono" :title="status.image.path">{{ status.image.path }}</span>
        </div>
      </template>
      <p v-else-if="status?.image_error" class="err">{{ status.image_error }}</p>
      <p v-else class="dim">正在读取安装包内的固件…</p>
    </section>

    <section class="card">
      <div class="card-head">
        <h2>USB 设备</h2>
        <span class="spacer" />
        <button class="sm" :disabled="busy !== null" @click="refresh">重新扫描</button>
      </div>

      <p v-if="!status?.ports.length" class="dim">
        没检测到设备。用数据线直连电脑（不要经无源 HUB），换个 USB 口再试；
        设备是空片也能识别——靠的是芯片内置的 USB，不是固件。
      </p>

      <div v-else class="ports">
        <div v-for="p in status?.ports" :key="p.path" class="port">
          <div class="port-main">
            <b class="mono">{{ p.path }}</b>
            <span class="tag" :data-tone="stateTone(p.path)">{{ stateLabel(p.path) }}</span>
          </div>

          <div v-if="busy?.port === p.path && busy.kind === 'flash'" class="progress">
            <div class="bar"><i :style="{ width: `${progress}%` }" /></div>
            <span class="mono">{{ progress }}%</span>
          </div>
          <div v-else class="actions">
            <button class="sm" :disabled="busy !== null" @click="probe(p.path)">
              {{ busy?.port === p.path ? '检测中…' : '检测版本' }}
            </button>
            <button
              class="sm primary"
              :disabled="!canFlash || probes[p.path]?.state === 'up_to_date'"
              @click="flash(p.path)"
            >
              {{ probes[p.path]?.first_flash ? '写入固件' : '重写固件' }}
            </button>
          </div>
        </div>
      </div>
    </section>

    <div v-if="error" class="result err-box">
      <strong class="mono">{{ error.code }}</strong>
      <span>{{ error.detail }}</span>
      <p class="dim">{{ errorHint }}</p>
    </div>

    <div v-if="report" class="result ok-box">
      <strong>写入完成，校验通过</strong>
      <span class="mono">
        {{ report.from_version ?? '空片' }} → {{ report.to_version }} ·
        {{ Math.round(report.bytes_written / 1024) }} KB ·
        {{ (report.elapsed_ms / 1000).toFixed(1) }} s
      </span>
      <p class="dim mono">app 分区 MD5 {{ report.md5 }}</p>
      <p class="dim">设备已复位，接下来去「实时」页扫描并连接。</p>
    </div>

    <details class="fold">
      <summary>烧写行为说明</summary>
      <p class="dim">
        烧写会复位设备，正在推流时不要点。只写 bootloader / 分区表 / app 三段，
        <b>不碰 NVS</b>——配对信息和按键映射会保留。中途拔线不会刷坏设备：重新插上再点一次即可。
      </p>
    </details>
  </div>
</template>

<style scoped>
.fw {
  display: flex;
  flex-direction: column;
  gap: var(--gap-card);
  min-height: 0;
  flex: 1 1 auto;
}

.card {
  display: flex;
  flex-direction: column;
  gap: var(--s2);
  flex: 0 0 auto;
}

.card-head {
  display: flex;
  align-items: center;
  gap: var(--s2);
}

.grid {
  display: grid;
  grid-template-columns: repeat(3, minmax(0, 1fr));
  gap: var(--s2);
}

.cell {
  background: var(--panel-2);
  border: 1px solid var(--line-soft);
  border-radius: var(--r-sm);
  padding: 7px 9px;
  display: flex;
  flex-direction: column;
  gap: 1px;
  min-width: 0;
}

.cell span {
  font-size: var(--fs-10);
  color: var(--dim);
}

.cell b {
  font-size: var(--fs-12);
  font-weight: 600;
  font-variant-numeric: tabular-nums;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.path-row {
  display: flex;
  align-items: center;
  gap: var(--s2);
  min-width: 0;
}

.path {
  font-size: var(--fs-10);
  color: var(--dim);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  min-width: 0;
  user-select: text;
}

.ports {
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.port {
  display: flex;
  align-items: center;
  gap: 10px;
  background: var(--panel-2);
  border: 1px solid var(--line-soft);
  border-radius: var(--r-sm);
  padding: 8px 10px;
}

.port-main {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
  flex: 1 1 auto;
}

.port-main b {
  font-size: var(--fs-11);
  font-weight: 500;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.tag {
  align-self: flex-start;
}

.tag[data-tone='ok'] {
  color: var(--ok);
  border-color: #3ddc9740;
  background: #3ddc9710;
}

.tag[data-tone='warn'] {
  color: var(--warn);
  border-color: #ffb02040;
  background: #ffb02010;
}

.actions {
  display: flex;
  align-items: center;
  gap: 6px;
  flex-shrink: 0;
}

.progress {
  display: flex;
  align-items: center;
  gap: var(--s2);
  flex-shrink: 0;
  font-size: var(--fs-11);
  color: var(--muted);
}

.bar {
  width: 110px;
  height: 5px;
  background: var(--bg);
  border: 1px solid var(--line);
  border-radius: 3px;
  overflow: hidden;
}

.bar i {
  display: block;
  height: 100%;
  background: var(--accent);
  transition: width 0.15s linear;
}

.result {
  border-radius: var(--r-sm);
  border: 1px solid var(--line);
  border-left-width: 3px;
  padding: 8px 10px;
  display: flex;
  flex-direction: column;
  gap: 3px;
  font-size: var(--fs-11);
}

.result strong {
  font-size: var(--fs-12);
}

.result p {
  margin: 0;
  line-height: 1.6;
}

.err-box {
  border-left-color: var(--err);
  background: #1f1518;
}

.ok-box {
  border-left-color: var(--ok);
  background: #121d18;
}

.err {
  color: var(--err);
  font-size: var(--fs-11);
  margin: 0;
}

.dim {
  color: var(--muted);
  font-size: var(--fs-11);
  line-height: 1.65;
  margin: 0;
}

.dim b {
  color: var(--text);
  font-weight: 600;
}

.fold > summary {
  cursor: pointer;
  font-size: var(--fs-10);
  letter-spacing: 0.05em;
  text-transform: uppercase;
  color: var(--dim);
  list-style: none;
  display: flex;
  align-items: center;
  gap: 6px;
}

.fold > summary::-webkit-details-marker {
  display: none;
}

.fold > summary::before {
  content: '';
  width: 6px;
  height: 6px;
  border-right: 1.4px solid var(--dim);
  border-bottom: 1.4px solid var(--dim);
  transform: rotate(-45deg);
  transition: transform 0.15s;
}

.fold[open] > summary::before {
  transform: rotate(45deg);
}

.fold[open] > p {
  margin: 6px 0 0;
  max-width: 760px;
}
</style>
