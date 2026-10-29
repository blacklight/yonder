import { computed, ref } from "vue";
import { defineStore } from "pinia";
import { getInstance } from "@/api/instance";
import type { InstanceEntity } from "@/api/entities";

// `/api/v2/instance` shared by every consumer — AppLayout's title,
// the invites gate, StatusCard's translate check. `ensure()` lazy-
// fetches once; the in-flight promise is per store instance so a
// fresh pinia (tests) refetches cleanly.
export const useInstanceStore = defineStore("instance", () => {
  const entity = ref<InstanceEntity | null>(null);
  const translationEnabled = computed(
    () => entity.value?.configuration?.translation?.enabled === true,
  );
  let inflight: Promise<InstanceEntity | null> | null = null;

  /** Fetch the instance entity once; failures stay unloaded — every
   * gate degrades to "feature off", matching a disabled backend. */
  async function ensure() {
    if (entity.value) return;
    inflight ??= getInstance()
      .then((i) => (entity.value = i))
      .catch(() => null);
    await inflight;
  }

  return { entity, translationEnabled, ensure };
});
