<script setup lang="ts">
import { onMounted, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { useListsStore } from "@/stores/lists";
import { listMembers } from "@/api/lists";
import { search } from "@/api/search";
import type { Account, ListEntity } from "@/api/entities";
import Avatar from "@/components/ui/Avatar.vue";
import AccountRowSkeleton from "@/components/ui/AccountRowSkeleton.vue";
import Button from "@/components/ui/Button.vue";
import Card from "@/components/ui/Card.vue";
import Input from "@/components/ui/Input.vue";
import Modal from "@/components/ui/Modal.vue";
import { FontAwesomeIcon } from "@/icons";

const { t } = useI18n();
const store = useListsStore();
const open = ref<string | null>(null);
const members = ref<Account[]>([]);
const membersLoading = ref(false);
const membersNext = ref<string | undefined>();
const form = ref({
  title: "",
  replies_policy: "followed" as const,
  exclusive: false,
});
const creating = ref(false);
const formError = ref<string | null>(null);
const memberQuery = ref("");
const memberHits = ref<Account[]>([]);
const deleting = ref<ListEntity | null>(null);

function cursor(url?: string) {
  if (!url) return undefined;
  return new URL(url, location.origin).searchParams.get("max_id") ?? undefined;
}

async function toggle(l: ListEntity) {
  if (open.value === l.id) {
    open.value = null;
    return;
  }
  open.value = l.id;
  memberQuery.value = "";
  memberHits.value = [];
  membersLoading.value = true;
  try {
    const page = await listMembers(l.id);
    members.value = page.items;
    membersNext.value = page.next;
  } finally {
    membersLoading.value = false;
  }
}

async function moreMembers() {
  if (!open.value || !membersNext.value) return;
  membersLoading.value = true;
  try {
    const page = await listMembers(open.value, {
      max_id: cursor(membersNext.value),
    });
    members.value = [...members.value, ...page.items];
    membersNext.value = page.next;
  } finally {
    membersLoading.value = false;
  }
}

async function create() {
  if (!form.value.title.trim() || creating.value) return;
  creating.value = true;
  formError.value = null;
  try {
    await store.create({ ...form.value });
    form.value.title = "";
  } catch {
    formError.value = t("common.error");
  } finally {
    creating.value = false;
  }
}

let memberSeq = 0;
let memberTimer: ReturnType<typeof setTimeout> | null = null;

async function findMembers() {
  const q = memberQuery.value.trim();
  const id = ++memberSeq;
  if (q.length < 2) {
    memberHits.value = [];
    return;
  }
  try {
    const hits = (
      await search({ q, type: "accounts", limit: 5, resolve: false })
    ).accounts;
    if (id === memberSeq) memberHits.value = hits;
  } catch {
    if (id === memberSeq) memberHits.value = [];
  }
}

// Debounce the member lookup — one request per keystroke otherwise.
watch(memberQuery, () => {
  if (memberTimer) clearTimeout(memberTimer);
  memberTimer = setTimeout(() => void findMembers(), 300);
});

async function addMember(id: string) {
  if (open.value) {
    await store.addMembers(open.value, [id]);
    members.value = (await listMembers(open.value)).items;
  }
}
async function removeMember(id: string) {
  if (open.value) {
    await store.removeMembers(open.value, [id]);
    members.value = members.value.filter((m) => m.id !== id);
  }
}
async function confirmDelete() {
  const l = deleting.value;
  deleting.value = null;
  if (l) await store.remove(l.id);
}

onMounted(() => store.fetch());
</script>

<template>
  <main class="page lists">
    <div class="page-head">
      <h1>{{ t("nav.lists") }}</h1>
    </div>

    <Card class="lists__create">
      <form class="lists__form" @submit.prevent="create">
        <Input v-model="form.title" :label="t('lists.titleLabel')" />
        <div class="lists__form-row">
          <label class="lists__field">
            <span class="lists__label">{{ t("lists.repliesPolicy") }}</span>
            <select
              v-model="form.replies_policy"
              class="lists__select"
              :aria-label="t('lists.repliesPolicy')"
            >
              <option value="followed">
                {{ t("lists.policyFollowed") }}
              </option>
              <option value="list">{{ t("lists.policyList") }}</option>
              <option value="none">{{ t("lists.policyNone") }}</option>
            </select>
          </label>
          <label class="lists__exclusive">
            <input v-model="form.exclusive" type="checkbox" />
            {{ t("lists.exclusive") }}
          </label>
        </div>
        <p v-if="formError" class="lists__error" role="alert">
          {{ formError }}
        </p>
        <div class="lists__form-actions">
          <Button
            type="submit"
            :loading="creating"
            :disabled="!form.title.trim()"
          >
            {{ t("lists.create") }}
          </Button>
        </div>
      </form>
    </Card>

    <ul
      v-if="store.loading && !store.lists.length"
      class="lists__list"
      role="status"
      :aria-label="t('common.loading')"
    >
      <li v-for="n in 3" :key="n" class="lists__item lists__item--skel">
        <AccountRowSkeleton :avatar="false" />
      </li>
    </ul>
    <p v-else-if="!store.lists.length" class="lists__state">
      {{ t("lists.empty") }}
    </p>
    <ul v-else class="lists__list">
      <li v-for="l in store.lists" :key="l.id" class="lists__item">
        <div class="lists__row">
          <FontAwesomeIcon icon="list-ul" class="lists__icon" />
          <router-link :to="`/lists/${l.id}`" class="lists__title">
            {{ l.title }}
          </router-link>
          <div class="lists__actions">
            <Button size="sm" variant="secondary" @click="toggle(l)">
              {{ t("lists.members") }}
            </Button>
            <Button size="sm" variant="danger" @click="deleting = l">
              {{ t("common.delete") }}
            </Button>
          </div>
        </div>
        <div v-if="open === l.id" class="lists__members">
          <Input
            v-model="memberQuery"
            :label="t('lists.addMember')"
            clearable
          />
          <ul v-if="memberHits.length" class="lists__people">
            <li v-for="a in memberHits" :key="a.id" class="lists__person">
              <Avatar :src="a.avatar" :name="a.acct" size="sm" />
              <span class="lists__acct">@{{ a.acct }}</span>
              <Button size="sm" @click="addMember(a.id)">
                {{ t("lists.add") }}
              </Button>
            </li>
          </ul>
          <ul
            v-if="membersLoading && !members.length"
            class="lists__people"
            role="status"
            :aria-label="t('common.loading')"
          >
            <li v-for="n in 2" :key="n"><AccountRowSkeleton /></li>
          </ul>
          <p v-else-if="!members.length" class="lists__members-empty">
            {{ t("lists.membersEmpty") }}
          </p>
          <ul v-else class="lists__people">
            <li v-for="m in members" :key="m.id" class="lists__person">
              <Avatar :src="m.avatar" :name="m.acct" size="sm" />
              <span class="lists__acct">@{{ m.acct }}</span>
              <Button size="sm" variant="danger" @click="removeMember(m.id)">
                {{ t("lists.remove") }}
              </Button>
            </li>
          </ul>
          <Button
            v-if="membersNext"
            size="sm"
            variant="secondary"
            :loading="membersLoading"
            @click="moreMembers"
          >
            {{ t("common.loadMore") }}
          </Button>
        </div>
      </li>
    </ul>
    <Modal
      v-if="deleting"
      :title="t('lists.deleteTitle')"
      @close="deleting = null"
    >
      <p>{{ t("lists.deleteBody") }}</p>
      <template #footer>
        <Button variant="secondary" @click="deleting = null">
          {{ t("ui.cancel") }}
        </Button>
        <Button variant="danger" @click="confirmDelete">
          {{ t("common.confirm") }}
        </Button>
      </template>
    </Modal>
  </main>
</template>

<style scoped>
.page-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-3);
  margin-bottom: var(--space-4);
}
.page-head h1 {
  margin: 0;
}
.lists__create {
  margin-bottom: var(--space-4);
}
.lists__form {
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
}
.lists__form-row {
  display: flex;
  align-items: flex-end;
  gap: var(--space-4);
  flex-wrap: wrap;
}
.lists__field {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
}
.lists__label {
  font-size: var(--text-sm);
  font-weight: 600;
  color: var(--text-muted);
}
.lists__select {
  padding: var(--space-2) var(--space-3);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  background: var(--bg-raised);
}
.lists__exclusive {
  display: inline-flex;
  align-items: center;
  gap: var(--space-2);
  padding-bottom: var(--space-2);
  font-size: var(--text-sm);
}
.lists__error {
  margin: 0;
  color: var(--danger);
  font-size: var(--text-sm);
}
.lists__form-actions {
  display: flex;
  justify-content: flex-end;
}
.lists__state {
  margin: 0;
  padding: var(--space-5) 0;
  text-align: center;
  color: var(--text-muted);
}
/* Rows mirror `.conv-list`: raised cards separated by dim borders
   inside a rounded container. */
.lists__list {
  list-style: none;
  margin: 0;
  padding: 0;
  background: var(--bg);
  border: 1px solid var(--border);
  border-radius: var(--radius-lg);
  overflow: hidden;
}
.lists__item {
  background: var(--bg-raised);
}
.lists__item + .lists__item {
  border-top: 1px solid var(--border-dim);
}
.lists__row {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  padding: var(--space-3) var(--space-4);
}
.lists__icon {
  color: var(--text-faint);
  flex-shrink: 0;
}
.lists__title {
  flex: 1;
  min-width: 0;
  font-weight: 600;
  color: var(--text);
  text-decoration: none;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.lists__title:hover {
  text-decoration: underline;
}
.lists__actions {
  display: flex;
  gap: var(--space-2);
  flex-shrink: 0;
}
.lists__members {
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
  padding: var(--space-3) var(--space-4) var(--space-4);
  border-top: 1px solid var(--border-dim);
  background: var(--bg-sunken);
}
.lists__people {
  list-style: none;
  margin: 0;
  padding: 0;
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
.lists__person {
  display: flex;
  align-items: center;
  gap: var(--space-3);
}
.lists__acct {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.lists__members-empty {
  margin: 0;
  color: var(--text-muted);
  font-size: var(--text-sm);
}
</style>
