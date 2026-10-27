<script setup lang="ts">
import { onMounted, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { useListsStore } from "@/stores/lists";
import { listMembers } from "@/api/lists";
import { search } from "@/api/search";
import type { Account, ListEntity } from "@/api/entities";
import Avatar from "@/components/ui/Avatar.vue";
import Button from "@/components/ui/Button.vue";
import Input from "@/components/ui/Input.vue";
import Modal from "@/components/ui/Modal.vue";

const { t } = useI18n();
const store = useListsStore();
const open = ref<string | null>(null);
const members = ref<Account[]>([]);
const membersNext = ref<string | undefined>();
const form = ref({
  title: "",
  replies_policy: "followed" as const,
  exclusive: false,
});
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
  const page = await listMembers(l.id);
  members.value = page.items;
  membersNext.value = page.next;
}

async function moreMembers() {
  if (!open.value || !membersNext.value) return;
  const page = await listMembers(open.value, {
    max_id: cursor(membersNext.value),
  });
  members.value = [...members.value, ...page.items];
  membersNext.value = page.next;
}

async function create() {
  if (!form.value.title.trim()) return;
  await store.create({ ...form.value });
  form.value.title = "";
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
  <main class="page">
    <h1>{{ t("nav.lists") }}</h1>
    <form @submit.prevent="create">
      <Input v-model="form.title" :label="t('lists.titleLabel')" />
      <select
        v-model="form.replies_policy"
        :aria-label="t('lists.repliesPolicy')"
      >
        <option value="followed">{{ t("lists.policyFollowed") }}</option>
        <option value="list">{{ t("lists.policyList") }}</option>
        <option value="none">{{ t("lists.policyNone") }}</option>
      </select>
      <label>
        <input v-model="form.exclusive" type="checkbox" />
        {{ t("lists.exclusive") }}
      </label>
      <Button type="submit">{{ t("lists.create") }}</Button>
    </form>
    <ul>
      <li v-for="l in store.lists" :key="l.id">
        <router-link :to="`/lists/${l.id}`">{{ l.title }}</router-link>
        <Button size="sm" variant="secondary" @click="toggle(l)">{{
          t("lists.members")
        }}</Button>
        <Button size="sm" variant="danger" @click="deleting = l">{{
          t("common.delete")
        }}</Button>
        <div v-if="open === l.id" class="members">
          <Input
            v-model="memberQuery"
            :label="t('lists.addMember')"
            clearable
          />
          <ul v-if="memberHits.length">
            <li v-for="a in memberHits" :key="a.id">
              <Avatar :src="a.avatar" :name="a.acct" size="sm" /> @{{ a.acct }}
              <Button size="sm" @click="addMember(a.id)">{{
                t("lists.add")
              }}</Button>
            </li>
          </ul>
          <ul>
            <li v-for="m in members" :key="m.id">
              <Avatar :src="m.avatar" :name="m.acct" size="sm" /> @{{ m.acct }}
              <Button size="sm" variant="danger" @click="removeMember(m.id)">
                {{ t("lists.remove") }}
              </Button>
            </li>
          </ul>
          <Button
            v-if="membersNext"
            size="sm"
            variant="secondary"
            @click="moreMembers"
          >
            {{ t("common.loadMore") }}
          </Button>
        </div>
      </li>
    </ul>
    <p v-if="!store.loading && !store.lists.length">{{ t("lists.empty") }}</p>
    <Modal
      v-if="deleting"
      :title="t('lists.deleteTitle')"
      @close="deleting = null"
    >
      <p>{{ t("lists.deleteBody") }}</p>
      <Button variant="danger" @click="confirmDelete">{{
        t("common.confirm")
      }}</Button>
    </Modal>
  </main>
</template>
