<script setup lang="ts">
import { computed, nextTick, onMounted, ref, watch, onUnmounted } from 'vue'
import { useQuestsStore } from '@/stores/quests'
import type { QuestRunView } from '@/stores/quests'
import { Card, CardHeader, CardTitle, CardContent } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { AlertCircle, ChevronUp, ListChecks, X, Square, Loader2 } from 'lucide-vue-next'
import { useI18n } from 'vue-i18n'

const { t } = useI18n()
const questsStore = useQuestsStore()
const expanded = ref(false)
const floatingRef = ref<HTMLElement | null>(null)
const floatingPosition = ref<{ left: number, top: number } | null>(null)
const draggedDuringPointer = ref(false)
const FLOATING_POSITION_KEY = 'questHelper_progressFloatingPosition'
const EDGE_MARGIN = 12

let dragState: {
  pointerId: number
  startX: number
  startY: number
  originLeft: number
  originTop: number
} | null = null

// Multi-run state
const activeRuns = computed(() => questsStore.activeRuns)
const stoppingRuns = ref<Set<string>>(new Set())

const hasFloatingContent = computed(() =>
  activeRuns.value.length > 0 || questsStore.questQueue.length > 0 || !!questsStore.error
)

const queuedUpcoming = computed(() => {
  const activeQuestIds = new Set(activeRuns.value.map(r => r.questId))
  return questsStore.questQueue.filter(quest => !activeQuestIds.has(quest.id))
})

const queuedBehindCount = computed(() => queuedUpcoming.value.length)

const floatingTitle = computed(() => {
  if (questsStore.error && activeRuns.value.length === 0) return t('toast.error')
  if (activeRuns.value.length > 0) {
    if (activeRuns.value.length === 1) return t('quest.active_progress')
    return `${activeRuns.value.length} ${t('quest.active_progress').toLowerCase()}`
  }
  return `${t('quest.up_next')} (${questsStore.questQueue.length})`
})

const floatingSubtitle = computed(() => {
  if (activeRuns.value.length > 0) {
    const firstRun = activeRuns.value[0]
    const quest = questsStore.quests.find(q => q.id === firstRun.questId)
    return quest?.config.messages.quest_name ?? t('quest.active_progress')
  }
  if (questsStore.questQueue.length > 0) {
    return questsStore.questQueue[0]?.config.messages.quest_name ?? t('quest.up_next')
  }
  return questsStore.error ?? ''
})

const floatingStyle = computed(() => {
  if (!floatingPosition.value) return {}
  return {
    left: `${floatingPosition.value.left}px`,
    top: `${floatingPosition.value.top}px`,
    right: 'auto',
    bottom: 'auto',
  }
})

function getQuestName(questId: string): string {
  const quest = questsStore.quests.find(q => q.id === questId)
  return quest?.config.messages.quest_name ?? 'Quest'
}

function getGameTitle(questId: string): string {
  const quest = questsStore.quests.find(q => q.id === questId)
  return quest?.config.messages.game_title ?? ''
}

function formatTime(seconds: number): string {
  const m = Math.floor(seconds / 60)
  const s = Math.floor(seconds % 60)
  return `${m}:${s.toString().padStart(2, '0')}`
}

function getTimeText(run: QuestRunView): string {
  const total = run.targetDuration
  const currentSeconds = (run.progress / 100) * total
  return `${formatTime(currentSeconds)} / ${formatTime(total)}`
}

async function handleStopRun(questId: string, runId: string) {
  stoppingRuns.value.add(runId)
  try {
    await questsStore.stopRun(questId, runId)
  } finally {
    stoppingRuns.value.delete(runId)
  }
}

async function handleStopAll() {
  await questsStore.stop()
}

function clampPosition(left: number, top: number) {
  const rect = floatingRef.value?.getBoundingClientRect()
  const width = rect?.width ?? 384
  const height = rect?.height ?? 80
  const maxLeft = Math.max(EDGE_MARGIN, window.innerWidth - width - EDGE_MARGIN)
  const maxTop = Math.max(EDGE_MARGIN, window.innerHeight - height - EDGE_MARGIN)

  return {
    left: Math.min(maxLeft, Math.max(EDGE_MARGIN, left)),
    top: Math.min(maxTop, Math.max(EDGE_MARGIN, top)),
  }
}

function saveFloatingPosition() {
  if (!floatingPosition.value) return
  localStorage.setItem(FLOATING_POSITION_KEY, JSON.stringify(floatingPosition.value))
}

async function reconcileFloatingPosition() {
  await nextTick()
  const rect = floatingRef.value?.getBoundingClientRect()
  if (!rect) return
  floatingPosition.value = clampPosition(rect.left, rect.top)
  saveFloatingPosition()
}

function startDrag(event: PointerEvent) {
  if (event.button !== 0) return
  const rect = floatingRef.value?.getBoundingClientRect()
  if (!rect) return

  dragState = {
    pointerId: event.pointerId,
    startX: event.clientX,
    startY: event.clientY,
    originLeft: rect.left,
    originTop: rect.top,
  }
  draggedDuringPointer.value = false

  if (event.currentTarget instanceof HTMLElement) {
    event.currentTarget.setPointerCapture(event.pointerId)
  }

  window.addEventListener('pointermove', handleDragMove)
  window.addEventListener('pointerup', stopDrag)
  window.addEventListener('pointercancel', stopDrag)
}

function handleDragMove(event: PointerEvent) {
  if (!dragState) return
  if (event.pointerId !== dragState.pointerId) return
  const dx = event.clientX - dragState.startX
  const dy = event.clientY - dragState.startY

  if (Math.abs(dx) + Math.abs(dy) > 4) {
    draggedDuringPointer.value = true
  }

  floatingPosition.value = clampPosition(dragState.originLeft + dx, dragState.originTop + dy)
}

function stopDrag(event: PointerEvent) {
  if (dragState && event.pointerId !== dragState.pointerId) return
  if (dragState && event.currentTarget instanceof HTMLElement) {
    try {
      event.currentTarget.releasePointerCapture(dragState.pointerId)
    } catch {
      // Pointer capture may already be released by the browser.
    }
  }

  dragState = null
  saveFloatingPosition()
  window.removeEventListener('pointermove', handleDragMove)
  window.removeEventListener('pointerup', stopDrag)
  window.removeEventListener('pointercancel', stopDrag)
}

function handleCollapsedClick() {
  if (draggedDuringPointer.value) {
    draggedDuringPointer.value = false
    return
  }
  expanded.value = true
}

onMounted(() => {
  const savedPosition = localStorage.getItem(FLOATING_POSITION_KEY)
  if (!savedPosition) return
  try {
    const parsed = JSON.parse(savedPosition) as { left?: unknown, top?: unknown }
    if (typeof parsed.left === 'number' && typeof parsed.top === 'number') {
      floatingPosition.value = clampPosition(parsed.left, parsed.top)
    }
  } catch {
    localStorage.removeItem(FLOATING_POSITION_KEY)
  }
})

watch(hasFloatingContent, (visible) => {
  if (!visible) expanded.value = false
})

watch([expanded, queuedBehindCount], () => {
  if (hasFloatingContent.value) {
    reconcileFloatingPosition()
  }
})

onUnmounted(() => {
  window.removeEventListener('pointermove', handleDragMove)
  window.removeEventListener('pointerup', stopDrag)
  window.removeEventListener('pointercancel', stopDrag)
})
</script>

<template>
  <div
    ref="floatingRef"
    v-if="hasFloatingContent"
    class="fixed bottom-5 right-5 z-50 w-[calc(100vw-2rem)] max-w-md"
    :style="floatingStyle"
  >
    <button
      v-if="!expanded"
      type="button"
      class="w-full cursor-grab rounded-lg border bg-card px-4 py-3 text-left shadow-lg transition-all hover:-translate-y-0.5 hover:shadow-xl active:cursor-grabbing focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
      @pointerdown="startDrag"
      @click="handleCollapsedClick"
    >
      <div class="flex items-center gap-3">
        <div class="flex h-9 w-9 shrink-0 items-center justify-center rounded-md bg-primary/10 text-primary">
          <AlertCircle v-if="questsStore.error && activeRuns.length === 0" class="h-4 w-4" />
          <ListChecks v-else class="h-4 w-4" />
        </div>
        <div class="min-w-0 flex-1">
          <div class="flex items-center justify-between gap-3">
            <p class="truncate text-sm font-semibold">{{ floatingTitle }}</p>
            <div class="flex shrink-0 items-center gap-2">
              <span v-if="queuedBehindCount > 0" class="rounded-full bg-secondary px-2 py-0.5 text-[11px] font-medium text-secondary-foreground">
                {{ t('home.queue_count', { count: queuedBehindCount }) }}
              </span>
              <span v-if="activeRuns.length > 0" class="text-sm font-semibold">
                {{ Math.floor(activeRuns[0].progress) }}%
              </span>
            </div>
          </div>
          <p class="mt-0.5 truncate text-xs text-muted-foreground">{{ floatingSubtitle }}</p>
          <div v-if="activeRuns.length > 0" class="mt-2 h-1.5 rounded-full bg-secondary">
            <div
              class="h-full rounded-full bg-primary transition-all duration-300"
              :style="{ width: `${activeRuns[0].progress}%` }"
            />
          </div>
        </div>
        <ChevronUp class="h-4 w-4 shrink-0 text-muted-foreground" />
      </div>
    </button>

    <Card v-else class="border-border/50 shadow-xl">
      <CardHeader
        class="flex cursor-grab flex-row items-center justify-between space-y-0 pb-3 active:cursor-grabbing"
        @pointerdown="startDrag"
      >
        <CardTitle class="min-w-0 truncate text-base">
          {{ floatingTitle }}
          <span v-if="queuedBehindCount > 0" class="ml-2 rounded-full bg-secondary px-2 py-0.5 text-xs font-medium text-secondary-foreground">
            {{ t('home.queue_count', { count: queuedBehindCount }) }}
          </span>
        </CardTitle>
        <Button variant="ghost" size="icon" class="h-8 w-8 shrink-0" @pointerdown.stop @click="expanded = false">
          <X class="h-4 w-4" />
        </Button>
      </CardHeader>
      <CardContent>
        <!-- Active Runs List -->
        <div v-if="activeRuns.length > 0" class="space-y-3">
          <div
            v-for="run in activeRuns"
            :key="run.runId"
            class="space-y-2 rounded-lg border bg-muted/30 p-3"
          >
            <div class="flex items-start justify-between gap-2">
              <div class="min-w-0 flex-1">
                <div class="truncate text-sm font-medium">{{ getQuestName(run.questId) }}</div>
                <div class="truncate text-xs text-muted-foreground">{{ getGameTitle(run.questId) }}</div>
                <div class="mt-1 flex items-center gap-2 text-xs text-muted-foreground">
                  <span class="font-mono">{{ getTimeText(run) }}</span>
                  <span v-if="run.phase === 'stopping'" class="rounded bg-amber-500/20 px-1.5 py-0.5 text-[10px] font-medium text-amber-600 dark:text-amber-400">
                    Stopping...
                  </span>
                </div>
              </div>
              <div class="flex shrink-0 items-center gap-2">
                <span class="text-sm font-semibold">{{ Math.floor(run.progress) }}%</span>
                <Button
                  variant="ghost"
                  size="icon"
                  class="h-7 w-7 shrink-0 text-destructive hover:text-destructive"
                  :disabled="stoppingRuns.has(run.runId) || run.phase === 'stopping'"
                  @click="handleStopRun(run.questId, run.runId)"
                >
                  <Square v-if="!stoppingRuns.has(run.runId) && run.phase !== 'stopping'" class="h-3 w-3" />
                  <Loader2 v-else class="h-3 w-3 animate-spin" />
                </Button>
              </div>
            </div>
            <div class="relative h-1.5 w-full rounded-full bg-secondary">
              <div
                class="absolute inset-y-0 left-0 rounded-full bg-primary transition-all duration-300"
                :style="{ width: `${run.progress}%` }"
              />
            </div>
          </div>

          <Button
            v-if="activeRuns.length > 1"
            variant="destructive"
            class="w-full gap-2"
            :disabled="questsStore.stopping"
            @click="handleStopAll"
          >
            <Loader2 v-if="questsStore.stopping" class="h-4 w-4 animate-spin" />
            {{ t('home.stop') }} All
          </Button>
          <Button
            v-else
            variant="destructive"
            class="w-full"
            :disabled="questsStore.stopping"
            @click="handleStopAll"
          >
            <Loader2 v-if="questsStore.stopping" class="h-4 w-4 mr-2 animate-spin" />
            {{ t('home.stop') }}
          </Button>
        </div>

        <!-- Queue Section -->
        <div v-if="queuedUpcoming.length > 0" :class="activeRuns.length > 0 && 'mt-4 border-t pt-4'">
          <div class="mb-2 flex items-center justify-between">
            <h4 class="text-sm font-semibold">{{ t('quest.up_next') }} ({{ queuedUpcoming.length }})</h4>
            <Button
              variant="ghost"
              size="sm"
              class="h-6 px-2 text-destructive hover:text-destructive"
              @click="questsStore.clearQueue"
            >
              {{ t('general.clear') }}
            </Button>
          </div>

          <div class="max-h-[260px] space-y-2 overflow-y-auto pr-1">
            <div
              v-for="(quest, index) in queuedUpcoming"
              :key="quest.id"
              class="flex items-center gap-2 rounded bg-muted/50 p-2 text-sm"
            >
              <span class="w-4 shrink-0 text-xs text-muted-foreground">{{ index + 1 }}.</span>
              <div class="min-w-0 flex-1">
                <div class="truncate font-medium">{{ quest.config.messages.quest_name }}</div>
                <div class="truncate text-xs text-muted-foreground">{{ quest.config.messages.game_title }}</div>
              </div>
            </div>
          </div>
        </div>

        <!-- Error Section -->
        <div v-if="questsStore.error" class="mt-4 flex items-start gap-2 rounded border border-red-500/20 bg-red-500/10 p-3 text-sm text-red-500">
          <AlertCircle class="mt-0.5 h-4 w-4 shrink-0" />
          <span class="break-words">{{ questsStore.error }}</span>
        </div>
      </CardContent>
    </Card>
  </div>
</template>
