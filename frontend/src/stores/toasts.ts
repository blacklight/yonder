import { defineStore } from "pinia";
import { ref } from "vue";
import type { IconProp } from "@fortawesome/fontawesome-svg-core";

export type ToastKind = "info" | "success" | "error";

export interface Toast {
  id: number;
  /** Primary line — semibold. */
  text: string;
  /** Optional secondary line — muted, two-line clamp. */
  body?: string;
  /** Route pushed when the toast body is activated. */
  to?: string;
  /** Semantic variant — drives the preset background. */
  kind?: ToastKind;
  /** FontAwesome icon shown left when no `image` is set. */
  icon?: IconProp;
  /** Leading image URL (e.g. the actor's avatar). */
  image?: string;
  /**
   * Custom background (any CSS color). Overrides the `kind` palette;
   * the foreground is computed for contrast.
   */
  background?: string;
}

/** `push("text")` stays valid; richer callers pass the full object. */
export type ToastInput = string | Omit<Toast, "id">;

const TIMEOUT_MS = 6000;
const MAX_TOASTS = 5;

/**
 * Transient in-app notifications (toasts). Producers `push` a message
 * (with an optional route target, icon/image, body and color);
 * [`Toasts`] renders the stack and each entry auto-dismisses after
 * [`TIMEOUT_MS`]. Oldest entries are evicted past [`MAX_TOASTS`] so a
 * burst can't flood the screen.
 */
export const useToastsStore = defineStore("toasts", () => {
  const items = ref<Toast[]>([]);
  let seq = 0;
  const timers = new Map<number, ReturnType<typeof setTimeout>>();

  function remove(id: number) {
    items.value = items.value.filter((t) => t.id !== id);
    const timer = timers.get(id);
    if (timer) {
      clearTimeout(timer);
      timers.delete(id);
    }
  }

  function push(input: ToastInput, to?: string): number {
    const id = ++seq;
    items.value.push({
      id,
      ...(typeof input === "string" ? { text: input, to } : input),
    });
    while (items.value.length > MAX_TOASTS) {
      remove(items.value[0].id);
    }
    timers.set(
      id,
      setTimeout(() => remove(id), TIMEOUT_MS),
    );
    return id;
  }

  return { items, push, remove };
});
