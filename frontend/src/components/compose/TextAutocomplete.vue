<script lang="ts">
import type { EmojiRef } from "@/utils/emoji";

/** One row in the inline autocomplete — shared shape for @mention,
 * :emoji and #hashtag suggestions. `insert` is the text that replaces
 * the trigger + token on selection. */
export interface AcItem {
  /** Unique id — option ids and keys. */
  id: string;
  /** Replacement text written over the trigger + token on select. */
  insert: string;
  /** Leading image (account avatar, custom emoji). */
  img?: string;
  /** Leading text glyph (unicode emoji, `#`), square-boxed like `img`. */
  glyph?: string;
  /** Primary label. */
  label: string;
  /** Custom emoji the label is rendered through (mention display
   * names). */
  labelEmojis?: EmojiRef[];
  /** Muted trailing text (e.g. `@acct`). */
  detail?: string;
}
</script>

<script setup lang="ts">
import { nextTick, onMounted, onUnmounted, onUpdated, ref, watch } from "vue";
import EmojiText from "@/components/ui/EmojiText.vue";

const props = defineProps<{
  items: AcItem[];
  active: number;
  textarea: HTMLTextAreaElement | null;
  /** Localized listbox label (`compose.*Suggestions`). */
  label: string;
}>();
const emit = defineEmits<{ select: [item: AcItem] }>();
const list = ref<HTMLElement | null>(null);

// Properties that decide where text wraps — the mirror div must wrap
// identically to the textarea for the marker offsets to be exact.
const MIRROR_PROPS = [
  "boxSizing",
  "width",
  "paddingTop",
  "paddingRight",
  "paddingBottom",
  "paddingLeft",
  "borderTopWidth",
  "borderRightWidth",
  "borderBottomWidth",
  "borderLeftWidth",
  "fontFamily",
  "fontSize",
  "fontWeight",
  "fontStyle",
  "letterSpacing",
  "textTransform",
  "textIndent",
  "lineHeight",
  "tabSize",
  "direction",
  "wordBreak",
  "overflowWrap",
] as const;

/** Pixel offset of the caret inside the textarea's visible box.
 * There is no DOM API for caret coordinates in a `<textarea>`, so
 * the text up to the caret is copied into a hidden mirror div with
 * identical box/font properties; a marker span at the caret index
 * then reports the position the real caret occupies. `lineHeight`
 * rides along so callers can place the list under the caret's line. */
function getTextareaCursorOffset(ta: HTMLTextAreaElement) {
  const style = getComputedStyle(ta);
  const div = document.createElement("div");
  const ds = div.style as unknown as Record<string, string>;
  for (const p of MIRROR_PROPS) ds[p] = style[p];
  ds.position = "absolute";
  ds.visibility = "hidden";
  ds.top = "0";
  ds.left = "0";
  ds.height = "auto";
  ds.overflow = "hidden";
  ds.whiteSpace = "pre-wrap";
  // A scrollbar narrows the wrap width — replicate it when the
  // textarea currently has one or the mirror wraps differently.
  if (ta.scrollHeight > ta.clientHeight) ds.overflowY = "scroll";

  const caret = ta.selectionEnd ?? ta.value.length;
  div.textContent = ta.value.slice(0, caret);
  const marker = document.createElement("span");
  // The next real char keeps trailing-whitespace wrap honest; a
  // zero-width space covers caret-at-EOF.
  marker.textContent = ta.value.slice(caret, caret + 1) || "\u200b";
  div.appendChild(marker);

  document.body.appendChild(div);
  const top = marker.offsetTop - ta.scrollTop;
  const left = marker.offsetLeft - ta.scrollLeft;
  const lineHeight =
    parseFloat(style.lineHeight) || parseFloat(style.fontSize) * 1.2;
  div.remove();
  return { top, left, lineHeight };
}

/**
 * Get the position of the autocomplete list depending on the current position
 * of the textarea cursor — relative to the *textarea's* box, not the
 * viewport, so a caret near the bottom of a tall box flips the list
 * upward even when the screen has room below.
 *
 * 1. Cursor in the top 75% + left half of the textarea ->
 *    autocomplete at bottom right of the cursor
 * 2. Cursor in the top 75% + right half of the textarea ->
 *    autocomplete at bottom left of the cursor
 * 3. Cursor in the bottom 25% + left half of the textarea ->
 *    autocomplete at top right of the cursor
 * 4. Cursor in the bottom 25% + right half of the textarea ->
 *    autocomplete at top left of the cursor
 */
function getListPosition() {
  const ta = props.textarea;
  if (!ta) return null;
  const caret = getTextareaCursorOffset(ta);
  // The caret offset is already in textarea-visible-box coordinates
  // (scrollTop was subtracted); clientHeight/clientWidth are the same
  // space. Both the quadrant tests and the anchor use it.
  const openBelow = caret.top < ta.clientHeight * 0.75;
  const openRight = caret.left < ta.clientWidth * 0.5;
  return {
    left: ta.offsetLeft + caret.left,
    // Below anchors the list's top under the caret line; above
    // anchors it at the line's top and translateY(-100%) lifts it.
    top: ta.offsetTop + caret.top + (openBelow ? caret.lineHeight : 0),
    flipX: !openRight,
    flipY: !openBelow,
  };
}

function reposition() {
  const el = list.value;
  const pos = getListPosition();
  if (!el || !pos) return;
  el.style.left = `${pos.left}px`;
  el.style.top = `${pos.top}px`;
  el.style.transform = `translate(${pos.flipX ? -100 : 0}%, ${pos.flipY ? -100 : 0}%)`;
}

onMounted(() => {
  reposition();
  // The caret moves without a re-render (arrows, clicks), so anchor
  // to the textarea's own events, not just Vue's update cycle.
  const ta = props.textarea;
  ta?.addEventListener("input", reposition);
  ta?.addEventListener("keyup", reposition);
  ta?.addEventListener("click", reposition);
  window.addEventListener("resize", reposition);
});
onUpdated(reposition);
onUnmounted(() => {
  const ta = props.textarea;
  ta?.removeEventListener("input", reposition);
  ta?.removeEventListener("keyup", reposition);
  ta?.removeEventListener("click", reposition);
  window.removeEventListener("resize", reposition);
});

// Keep the highlighted row visible while arrowing through the list.
// `scrollIntoView` is absent under jsdom — guard with optional call.
watch(
  () => props.active,
  async (i) => {
    await nextTick();
    (list.value?.children[i] as HTMLElement | undefined)?.scrollIntoView?.({
      block: "nearest",
    });
  },
);
</script>

<template>
  <ul
    id="text-ac"
    ref="list"
    class="text-ac"
    role="listbox"
    :aria-label="label"
    data-testid="text-ac"
  >
    <li
      v-for="(item, i) in items"
      :id="`text-ac-opt-${item.id}`"
      :key="item.id"
      role="option"
      :aria-selected="i === active"
      class="text-ac__item"
      :class="{ 'text-ac__item--active': i === active }"
      @mousedown.prevent="emit('select', item)"
    >
      <img v-if="item.img" :src="item.img" alt="" class="text-ac__img" />
      <span v-else-if="item.glyph" class="text-ac__glyph">{{
        item.glyph
      }}</span>
      <span class="text-ac__name"
        ><EmojiText
          v-if="item.labelEmojis"
          :text="item.label"
          :emojis="item.labelEmojis"
        /><template v-else>{{ item.label }}</template></span
      >
      <span v-if="item.detail" class="text-ac__detail" dir="ltr">{{
        item.detail
      }}</span>
    </li>
  </ul>
</template>

<style scoped>
.text-ac {
  /* Absolutely positioned at the caret inside the relatively-
     positioned .compose__text-wrap; inline top/left/transform come
     from reposition(). */
  position: absolute;
  list-style: none;
  margin: 0;
  padding: var(--space-1);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  background: var(--bg-raised);
  box-shadow: 0 4px 16px rgb(0 0 0 / 15%);
  max-height: 16rem;
  max-width: 100%;
  overflow-y: auto;
  z-index: 20;
}
.text-ac__item {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  padding: var(--space-2) var(--space-3);
  border-radius: var(--radius-sm);
  cursor: pointer;
}
.text-ac__item--active {
  background: var(--accent-soft);
}
.text-ac__img {
  width: 2rem;
  height: 2rem;
  border-radius: var(--radius-sm);
  object-fit: cover;
  flex: 0 0 auto;
}
/* Same 2rem square the image gets — text glyphs (unicode emoji,
 * `#`) box identically instead of shifting the label column. */
.text-ac__glyph {
  width: 2rem;
  height: 2rem;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  font-size: 1.5rem;
  line-height: 1;
  flex: 0 0 auto;
}
.text-ac__name {
  font-weight: 600;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.text-ac__detail {
  color: var(--text-muted);
  font-size: var(--text-sm);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  margin-inline-start: auto;
}
</style>
