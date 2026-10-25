import { defineStore } from "pinia";
import { ref } from "vue";
import {
  addListMembers,
  createList,
  deleteList,
  listLists,
  removeListMembers,
  updateList,
  type ListBody,
} from "@/api/lists";
import type { ListEntity } from "@/api/entities";
import { resetOnUserChange } from "./perUser";

/** The caller's lists — loaded once, mutated locally on success. */
export const useListsStore = defineStore("lists", () => {
  const lists = ref<ListEntity[]>([]);
  const loading = ref(false);
  const error = ref<string | null>(null);
  const loaded = ref(false);

  function reset() {
    lists.value = [];
    error.value = null;
    loaded.value = false;
  }
  resetOnUserChange(reset);

  async function fetch() {
    loading.value = true;
    error.value = null;
    try {
      lists.value = await listLists();
      loaded.value = true;
    } catch (e) {
      error.value = e instanceof Error ? e.message : String(e);
    } finally {
      loading.value = false;
    }
  }

  async function create(body: ListBody) {
    const list = await createList(body);
    lists.value.push(list);
    return list;
  }

  async function update(id: string, body: ListBody) {
    const list = await updateList(id, body);
    const i = lists.value.findIndex((l) => l.id === id);
    if (i >= 0) lists.value[i] = list;
    return list;
  }

  async function remove(id: string) {
    await deleteList(id);
    lists.value = lists.value.filter((l) => l.id !== id);
  }

  async function addMembers(listId: string, accountIds: string[]) {
    await addListMembers(listId, accountIds);
  }
  async function removeMembers(listId: string, accountIds: string[]) {
    await removeListMembers(listId, accountIds);
  }

  return {
    lists,
    loading,
    error,
    loaded,
    reset,
    fetch,
    create,
    update,
    remove,
    addMembers,
    removeMembers,
  };
});
