<script setup lang="ts">
// "Copy page as Markdown" and "Open as Markdown": every page's Markdown is
// published next to it (scripts/llms.mjs), for pasting into an AI tool.
import { computed, ref } from 'vue'
import { useData, withBase } from 'vitepress'

const { page } = useData()
const href = computed(() => withBase(`/${page.value.relativePath}`))
const status = ref('')

async function copy() {
  try {
    const response = await fetch(href.value)
    if (!response.ok) throw new Error(String(response.status))
    await navigator.clipboard.writeText(await response.text())
    status.value = 'Copied the page as Markdown.'
  } catch {
    status.value = 'Could not copy; open the Markdown instead.'
  }
  setTimeout(() => (status.value = ''), 4000)
}
</script>

<template>
  <div class="markdown-actions">
    <button type="button" class="markdown-action" @click="copy">Copy page as Markdown</button>
    <a class="markdown-action" :href="href" target="_blank" rel="noopener">Open as Markdown</a>
    <span class="markdown-status" role="status" aria-live="polite">{{ status }}</span>
  </div>
</template>

<style scoped>
.markdown-actions {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px 12px;
  margin-bottom: 16px;
  font-size: 0.8125rem;
}

.markdown-action {
  border: 1px solid var(--vp-c-divider);
  border-radius: 6px;
  padding: 2px 10px;
  color: var(--vp-c-text-1);
  background-color: var(--vp-c-bg-alt);
  text-decoration: none;
  line-height: 1.6;
  cursor: pointer;
}

.markdown-action:hover {
  border-color: var(--vp-c-brand-1);
}

.markdown-status {
  color: var(--vp-c-text-2);
}
</style>
