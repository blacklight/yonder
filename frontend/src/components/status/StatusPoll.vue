<script setup lang="ts">
import { computed, ref } from "vue";
import { useI18n } from "vue-i18n";
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
      <label v-for="(opt, idx) in poll.options" :key="idx" class="poll__option">
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
      <div v-for="(opt, idx) in poll.options" :key="idx" class="poll__result">
        <span class="poll__result-title">{{ opt.title }}</span>
        <span class="poll__result-count">{{ opt.votes_count ?? "—" }}</span>
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
}
.poll__vote {
  align-self: flex-start;
  border: 1px solid var(--border);
  background: var(--bg-raised);
  border-radius: var(--radius-md);
  padding: var(--space-1) var(--space-3);
  color: var(--text);
  cursor: pointer;
  font: inherit;
}
.poll__vote:disabled {
  opacity: 0.5;
  cursor: default;
}
.poll__result {
  display: flex;
  justify-content: space-between;
  gap: var(--space-2);
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
