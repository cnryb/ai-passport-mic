<script setup lang="ts">
import type { DeviceView } from '../lib/types'

defineProps<{
  devices: DeviceView[]
  connectedId: string | null
  busy: boolean
  scanning: boolean
}>()

const emit = defineEmits<{ (e: 'connect', id: string): void }>()

function rssiTone(rssi: number | null): string {
  if (rssi === null) return 'unknown'
  if (rssi >= -55) return 'strong'
  if (rssi >= -70) return 'ok'
  return 'weak'
}

function shortId(id: string): string {
  return id.length > 12 ? `${id.slice(0, 8)}…` : id
}
</script>

<template>
  <div class="list">
    <p v-if="devices.length === 0" class="empty">
      <template v-if="scanning">正在扫描…设备会在发现后立刻出现</template>
      <template v-else>还没有设备。点「扫描」开始；设备需上电并且未被其他主机连接。</template>
    </p>

    <div v-for="d in devices" :key="d.id" class="row" :data-connected="d.id === connectedId">
      <div class="id">
        <span class="name">{{ d.name || '（未命名设备）' }}</span>
        <span class="sub mono">{{ shortId(d.id) }}</span>
      </div>

      <span class="badge" :class="{ muted: !d.has_service }">{{ d.has_service ? '本项目' : '其他' }}</span>

      <span class="rssi" :data-tone="rssiTone(d.rssi)">
        <i class="bars"><b /><b /><b /></i>
        {{ d.rssi === null ? '—' : `${d.rssi} dBm` }}
      </span>

      <button v-if="d.id === connectedId" class="sm ghost" disabled>已连接</button>
      <button
        v-else
        class="sm"
        :disabled="busy || connectedId !== null"
        @click="emit('connect', d.id)"
      >
        连接
      </button>
    </div>
  </div>
</template>

<style scoped>
.list {
  display: flex;
  flex-direction: column;
  gap: 2px;
  max-height: 168px;
  overflow-y: auto;
  scrollbar-width: thin;
}

.empty {
  color: var(--dim);
  font-size: var(--fs-11);
  padding: 14px 4px;
  margin: 0;
  line-height: 1.7;
}

.row {
  display: grid;
  grid-template-columns: minmax(0, 1fr) auto auto auto;
  align-items: center;
  gap: 10px;
  padding: 7px 8px;
  border-radius: var(--r-sm);
  border: 1px solid transparent;
}

.row:hover {
  background: #171d26;
  border-color: var(--line-soft);
}

.row[data-connected='true'] {
  background: #3ddc970c;
  border-color: #3ddc9733;
}

.id {
  display: flex;
  flex-direction: column;
  min-width: 0;
}

.name {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-size: var(--fs-12);
  font-weight: 500;
}

.sub {
  color: var(--dim);
  font-size: var(--fs-10);
}

.badge {
  font-size: var(--fs-10);
  padding: 1px 7px;
  border-radius: var(--r-pill);
  background: #4c8dff1c;
  color: #9ec3ff;
  white-space: nowrap;
}

.badge.muted {
  background: #ffffff0a;
  color: var(--dim);
}

.rssi {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  font-size: var(--fs-11);
  color: var(--muted);
  font-variant-numeric: tabular-nums;
  min-width: 68px;
  justify-content: flex-end;
}

.bars {
  display: inline-flex;
  align-items: flex-end;
  gap: 2px;
  height: 11px;
}

.bars b {
  width: 3px;
  background: var(--dim);
  border-radius: 1px;
}

.bars b:nth-child(1) {
  height: 4px;
}

.bars b:nth-child(2) {
  height: 7px;
}

.bars b:nth-child(3) {
  height: 11px;
}

.rssi[data-tone='strong'] .bars b {
  background: var(--ok);
}

.rssi[data-tone='ok'] .bars b:nth-child(1),
.rssi[data-tone='ok'] .bars b:nth-child(2) {
  background: var(--ok);
}

.rssi[data-tone='weak'] .bars b:nth-child(1) {
  background: var(--warn);
}

.rssi[data-tone='unknown'] .bars {
  opacity: 0.35;
}

button {
  min-width: 58px;
}
</style>
