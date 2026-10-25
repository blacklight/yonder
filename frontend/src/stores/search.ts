import { defineStore } from "pinia";
import { ref } from "vue";
import { search, type SearchResults } from "@/api/search";
import { resetOnUserChange } from "./perUser";

/**
 * Search state: the submitted query's results plus debounced
 * autocomplete suggestions. Two rules from the backend contract:
 * autocomplete NEVER sends `resolve=true` (a remote fetch per
 * keystroke) and prefix mode requires a single `type`, so suggestions
 * issue two parallel requests (accounts + hashtags). A monotonic
 * request id guards against stale responses racing the input.
 */
export const useSearchStore = defineStore("search", () => {
  const results = ref<SearchResults | null>(null);
  const suggestions = ref<{ accounts: SearchResults["accounts"]; hashtags: SearchResults["hashtags"] } | null>(null);
  const loading = ref(false);
  const suggestLoading = ref(false);
  const error = ref<string | null>(null);
  let seq = 0;
  let timer: ReturnType<typeof setTimeout> | null = null;

  resetOnUserChange(() => {
    results.value = null;
    suggestions.value = null;
  });

  /** Full submitted search; `resolve` only from an explicit caller. */
  async function run(q: string, resolve = false) {
    const id = ++seq;
    loading.value = true;
    error.value = null;
    try {
      const r = await search({ q, limit: 40, resolve });
      if (id === seq) results.value = r;
    } catch (e) {
      if (id === seq) {
        error.value = e instanceof Error ? e.message : String(e);
        results.value = null;
      }
      // Callers distinguish 401 (anonymous search disabled) from
      // ordinary failures — record the error AND rethrow.
      throw e;
    } finally {
      if (id === seq) loading.value = false;
    }
  }

  /** Debounced autocomplete — prefix mode, `resolve=false` always. */
  function suggest(q: string, delayMs = 300) {
    if (timer) clearTimeout(timer);
    if (q.trim().length < 2) {
      suggestions.value = null;
      return;
    }
    timer = setTimeout(() => {
      timer = null;
      void fetchSuggestions(q.trim());
    }, delayMs);
  }

  async function fetchSuggestions(q: string) {
    const id = ++seq;
    suggestLoading.value = true;
    try {
      const [accounts, hashtags] = await Promise.all([
        search({ q, type: "accounts", limit: 5, resolve: false }),
        search({ q, type: "hashtags", limit: 5, resolve: false }),
      ]);
      if (id === seq)
        suggestions.value = {
          accounts: accounts.accounts,
          hashtags: hashtags.hashtags,
        };
    } catch {
      if (id === seq) suggestions.value = null;
    } finally {
      if (id === seq) suggestLoading.value = false;
    }
  }

  function clearSuggestions() {
    suggestions.value = null;
    if (timer) {
      clearTimeout(timer);
      timer = null;
    }
  }

  return { results, suggestions, loading, suggestLoading, error, run, suggest, clearSuggestions };
});
