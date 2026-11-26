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
  const suggestions = ref<{
    accounts: SearchResults["accounts"];
    hashtags: SearchResults["hashtags"];
  } | null>(null);
  const suggestionOwner = ref<string | null>(null);
  const loading = ref(false);
  const suggestLoading = ref(false);
  const error = ref<string | null>(null);
  let resultSeq = 0;
  let suggestionSeq = 0;
  let timer: ReturnType<typeof setTimeout> | null = null;

  resetOnUserChange(() => {
    clearResults();
    clearSuggestions();
  });

  /** Full submitted search; `resolve` only from an explicit caller. */
  async function run(q: string, resolve = false) {
    const id = ++resultSeq;
    loading.value = true;
    error.value = null;
    try {
      const r = await search({ q, limit: 40, resolve });
      if (id === resultSeq) results.value = r;
    } catch (e) {
      if (id === resultSeq) {
        error.value = e instanceof Error ? e.message : String(e);
        results.value = null;
      }
      // Callers distinguish 401 (anonymous search disabled) from
      // ordinary failures — record the error AND rethrow.
      throw e;
    } finally {
      if (id === resultSeq) loading.value = false;
    }
  }

  /** Debounced autocomplete — prefix mode, `resolve=false` always. */
  function suggest(q: string, delayMs = 300, owner = "default") {
    if (timer) clearTimeout(timer);
    suggestionSeq++;
    if (q.trim().length < 2) {
      clearSuggestions();
      return;
    }
    suggestionOwner.value = owner;
    timer = setTimeout(() => {
      timer = null;
      void fetchSuggestions(q.trim(), owner);
    }, delayMs);
  }

  async function fetchSuggestions(q: string, owner: string) {
    const id = ++suggestionSeq;
    suggestLoading.value = true;
    try {
      const [accounts, hashtags] = await Promise.all([
        search({ q, type: "accounts", limit: 5, resolve: false }),
        search({ q, type: "hashtags", limit: 5, resolve: false }),
      ]);
      if (id === suggestionSeq) {
        suggestions.value = {
          accounts: accounts.accounts,
          hashtags: hashtags.hashtags,
        };
        suggestionOwner.value = owner;
      }
    } catch {
      if (id === suggestionSeq) {
        suggestions.value = null;
        suggestionOwner.value = null;
      }
    } finally {
      if (id === suggestionSeq) suggestLoading.value = false;
    }
  }

  function clearSuggestions() {
    suggestionSeq++;
    suggestions.value = null;
    suggestionOwner.value = null;
    suggestLoading.value = false;
    if (timer) {
      clearTimeout(timer);
      timer = null;
    }
  }

  function clearResults() {
    resultSeq++;
    results.value = null;
    loading.value = false;
    error.value = null;
  }

  return {
    results,
    suggestions,
    suggestionOwner,
    loading,
    suggestLoading,
    error,
    run,
    suggest,
    clearSuggestions,
    clearResults,
  };
});
