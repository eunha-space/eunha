<script setup lang="ts">
import { computed, ref } from "vue";
import type { Divergence, Kind } from "../../mastodon/divergences.data";

const props = defineProps<{ divergences: Divergence[] }>();

const source = "https://github.com/limeburst/eunha/blob/main/";
const kinds: { kind: Kind; label: string; hint: string }[] = [
  { kind: "addition", label: "Additions", hint: "eunha does something Mastodon does not" },
  { kind: "omission", label: "Omissions", hint: "Mastodon does something eunha does not" },
  { kind: "behaviour", label: "Behaviour", hint: "both do it, differently" },
];

const shown = ref<Kind | null>(null);
const query = ref("");

const count = (kind: Kind) =>
  props.divergences.filter((d) => d.kind === kind).length;

// Searches what the reader sees: the id and each field, tags stripped.
const text = (d: Divergence) =>
  [d.id, d.summary, d.mastodon, d.eunha, d.why, d.evidence]
    .join(" ")
    .replace(/<[^>]*>/g, "")
    .toLowerCase();

const matching = computed(() => {
  const words = query.value.toLowerCase().split(/\s+/).filter(Boolean);
  return props.divergences.filter(
    (d) =>
      (shown.value === null || d.kind === shown.value) &&
      words.every((word) => text(d).includes(word)),
  );
});
</script>

<template>
  <div class="divergences">
    <div class="controls">
      <div class="kinds" role="group" aria-label="Kind">
        <button
          type="button"
          :aria-pressed="shown === null"
          @click="shown = null"
        >
          All <span class="count">{{ divergences.length }}</span>
        </button>
        <button
          v-for="k in kinds"
          :key="k.kind"
          type="button"
          :title="k.hint"
          :aria-pressed="shown === k.kind"
          @click="shown = shown === k.kind ? null : k.kind"
        >
          {{ k.label }} <span class="count">{{ count(k.kind) }}</span>
        </button>
      </div>
      <input
        v-model="query"
        type="search"
        placeholder="Search divergences"
        aria-label="Search divergences"
      />
    </div>

    <p v-if="matching.length === 0" class="none">No divergence matches.</p>

    <article v-for="d in matching" :id="d.id" :key="d.id" class="entry">
      <header>
        <span class="kind" :data-kind="d.kind">{{ d.kind }}</span>
        <a class="id" :href="`#${d.id}`"><code>{{ d.id }}</code></a>
      </header>
      <p class="summary" v-html="d.summary" />
      <dl>
        <dt>Mastodon</dt>
        <dd v-html="d.mastodon" />
        <dt>Eunha</dt>
        <dd v-html="d.eunha" />
        <dt>Why</dt>
        <dd v-html="d.why" />
        <dt>Evidence</dt>
        <dd>
          <a :href="source + d.evidence"><code>{{ d.evidence }}</code></a>
        </dd>
      </dl>
      <footer>
        Since Mastodon {{ d.since }}, last reviewed for {{ d.reviewedFor }}
      </footer>
    </article>
  </div>
</template>

<style scoped>
.controls {
  display: flex;
  flex-wrap: wrap;
  gap: 12px;
  align-items: center;
  margin: 24px 0 16px;
}
.kinds {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
}
.kinds button {
  border: 1px solid var(--vp-c-divider);
  border-radius: 999px;
  padding: 2px 12px;
  font-size: 14px;
  color: var(--vp-c-text-2);
  background: var(--vp-c-bg);
}
.kinds button[aria-pressed="true"] {
  border-color: var(--vp-c-brand-1);
  color: var(--vp-c-brand-1);
  background: var(--vp-c-brand-soft);
}
.count {
  color: var(--vp-c-text-3);
  font-variant-numeric: tabular-nums;
}
input[type="search"] {
  flex: 1 1 200px;
  min-width: 0;
  border: 1px solid var(--vp-c-divider);
  border-radius: 8px;
  padding: 6px 10px;
  font-size: 14px;
  background: var(--vp-c-bg);
  color: var(--vp-c-text-1);
}
input[type="search"]:focus {
  border-color: var(--vp-c-brand-1);
  outline: none;
}
.none {
  color: var(--vp-c-text-2);
}
.entry {
  border: 1px solid var(--vp-c-divider);
  border-radius: 12px;
  padding: 16px 20px;
  margin: 16px 0;
  scroll-margin-top: calc(var(--vp-nav-height) + 16px);
}
.entry:target {
  border-color: var(--vp-c-brand-1);
}
.entry header {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
  align-items: center;
}
.kind {
  border-radius: 4px;
  padding: 0 6px;
  font-size: 12px;
  font-weight: 600;
  text-transform: uppercase;
  letter-spacing: 0.04em;
  color: var(--vp-c-text-2);
  background: var(--vp-c-default-soft);
}
.kind[data-kind="addition"] {
  color: var(--vp-c-green-1);
  background: var(--vp-c-green-soft);
}
.kind[data-kind="omission"] {
  color: var(--vp-c-yellow-1);
  background: var(--vp-c-yellow-soft);
}
.kind[data-kind="behaviour"] {
  color: var(--vp-c-indigo-1);
  background: var(--vp-c-indigo-soft);
}
.id {
  text-decoration: none;
  overflow-wrap: anywhere;
}
.summary {
  margin: 8px 0 4px;
  font-weight: 600;
  color: var(--vp-c-text-1);
}
dl {
  margin: 8px 0 0;
}
dt {
  margin-top: 10px;
  font-size: 13px;
  font-weight: 600;
  color: var(--vp-c-text-2);
}
dd {
  margin: 2px 0 0;
  overflow-wrap: anywhere;
}
footer {
  margin-top: 12px;
  font-size: 13px;
  color: var(--vp-c-text-3);
}
</style>
