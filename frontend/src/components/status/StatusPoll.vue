<script setup lang="ts">
import { computed, ref } from "vue";
import { useI18n } from "vue-i18n";
import { FontAwesomeIcon } from "@/icons";
import { votePoll } from "@/api/statuses";
import { ApiError } from "@/api/errors";
import { formatAbsoluteDate } from "@/utils/relativeDate";
import type { Status } from "@/api/entities";

const props = defineProps<{ status: Status }>();
const { t } = useI18n();

const poll = computed(() => props.status.poll);
const chosen = ref<number[]>([]);
const voting = ref(false);
const error = ref("");
const voted = ref(false);

const showResults = computed(
  () => !!poll.value && (poll.value.expired || poll.value.voted || voted.value),
);

// Bars are scaled against the option with the most votes so the leader
// always spans the full row; ties share the full width.
const maxVotes = computed(() =>
  (poll.value?.options ?? []).reduce(
    (max, o) => Math.max(max, o.votes_count ?? 0),
    0,
  ),
);

function isWinner(idx: number): boolean {
  const votes = poll.value?.options[idx]?.votes_count;
  return votes != null && maxVotes.value > 0 && votes === maxVotes.value;
}

function barWidth(votes: number | null): string {
  if (votes == null || maxVotes.value === 0) return "0%";
  return `${(votes / maxVotes.value) * 100}%`;
}

// Option votes as a share of the poll total — for single-choice polls
// they sum to 100%, for multiple-choice they reflect total ballots.
function percent(votes: number | null): number {
  const total = poll.value?.votes_count ?? 0;
  if (votes == null || total === 0) return 0;
  return Math.round((votes / total) * 100);
}

function toggle(idx: number) {
  if (!poll.value) return;
  if (poll.value.multiple) {
    const i = chosen.value.indexOf(idx);
    if (i === -1) chosen.value.push(idx);
    else chosen.value.splice(i, 1);
  } else {
    chosen.value = [idx];
  }
}

async function vote() {
  if (!poll.value || chosen.value.length === 0) return;
  voting.value = true;
  error.value = "";
  try {
    const updated = await votePoll(poll.value.id, chosen.value);
    // The response carries fresh counts — write it back onto the
    // shared timeline entity (same deliberate-mutation pattern as
    // StatusFooter) so results aren't stale until reload.
    // eslint-disable-next-line vue/no-mutating-props
    props.status.poll = updated;
    voted.value = true;
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : t("status.pollError");
  } finally {
    voting.value = false;
  }
}
</script>

<template>
  <div v-if="poll" class="poll" data-testid="poll">
    <template v-if="!showResults">
      <label
        v-for="(opt, idx) in poll.options"
        :key="idx"
        class="poll__option"
        :class="{ 'poll__option--selected': chosen.includes(idx) }"
      >
        <input
          :type="poll.multiple ? 'checkbox' : 'radio'"
          :name="`poll-${poll.id}`"
          :checked="chosen.includes(idx)"
          :disabled="voting"
          @change="toggle(idx)"
        />
        <span>{{ opt.title }}</span>
      </label>
      <p v-if="error" class="poll__error">{{ error }}</p>
      <button
        type="button"
        class="poll__vote"
        :disabled="voting || chosen.length === 0"
        @click="vote"
      >
        {{ t("status.vote") }}
      </button>
    </template>
    <template v-else>
      <div
        v-for="(opt, idx) in poll.options"
        :key="idx"
        class="poll__result"
        :class="{ 'poll__result--winner': isWinner(idx) }"
      >
        <div
          class="poll__result-bar"
          :style="{ width: barWidth(opt.votes_count) }"
          aria-hidden="true"
        />
        <span class="poll__result-title">
          <FontAwesomeIcon
            v-if="poll.expired && isWinner(idx)"
            icon="circle-check"
            class="poll__result-check"
          />
          <span class="poll__result-name">{{ opt.title }}</span>
        </span>
        <span class="poll__result-count">
          <span class="poll__result-percent"
            >{{ percent(opt.votes_count) }}%</span
          >
          <span class="poll__result-votes">{{ opt.votes_count ?? "—" }}</span>
        </span>
      </div>
      <p class="poll__meta">
        {{ t("status.pollVotes", { count: poll.votes_count }) }}
        <template v-if="poll.expires_at">
          ·
          {{
            poll.expired
              ? t("status.pollClosed")
              : t("status.pollEnds", {
                  at: formatAbsoluteDate(poll.expires_at),
                })
          }}
        </template>
      </p>
    </template>
  </div>
</template>

<style scoped>
.poll {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
  margin: var(--space-2) 0;
}
.poll__option {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  padding: var(--space-2) var(--space-3);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  cursor: pointer;
  transition:
    border-color 0.15s ease,
    background 0.15s ease;
}
.poll__option:hover {
  border-color: var(--accent);
}
.poll__option--selected,
.poll__option--selected:hover {
  border-color: var(--accent);
  background: var(--accent-soft);
}
.poll__option input {
  accent-color: var(--accent);
  margin: 0;
}
.poll__vote {
  align-self: flex-start;
  border: 0;
  background: var(--accent);
  border-radius: var(--radius-md);
  padding: var(--space-1) var(--space-3);
  color: var(--accent-fg);
  cursor: pointer;
  font: inherit;
}
.poll__vote:disabled {
  opacity: 0.5;
  cursor: default;
}
.poll__result {
  position: relative;
  display: flex;
  justify-content: space-between;
  align-items: center;
  gap: var(--space-2);
  padding: var(--space-2) var(--space-3);
  border-radius: var(--radius-md);
  overflow: hidden;
}
.poll__result--winner {
  font-weight: 600;
}
.poll__result-bar {
  position: absolute;
  inset-block: 0;
  left: 0;
  background: var(--accent-soft);
  transition: width 0.3s ease;
}
.poll__result--winner .poll__result-bar {
  background: color-mix(in srgb, var(--accent) 35%, transparent);
}
.poll__result-title {
  position: relative;
  display: flex;
  align-items: center;
  gap: var(--space-2);
  min-width: 0;
}
.poll__result-name {
  overflow: hidden;
  text-overflow: ellipsis;
}
.poll__result-check {
  color: var(--accent-link);
  flex-shrink: 0;
}
.poll__result-count {
  position: relative;
  display: flex;
  align-items: baseline;
  gap: var(--space-1);
  white-space: nowrap;
}
.poll__result-votes {
  color: var(--text-muted);
  font-size: var(--text-sm);
  font-weight: 400;
}
.poll__meta,
.poll__error {
  color: var(--text-muted);
  font-size: var(--text-sm);
  margin: 0;
}
.poll__error {
  color: var(--danger, #c0392b);
}
</style>
