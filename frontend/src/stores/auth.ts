import { defineStore } from "pinia";

const TOKEN_KEY = "yonder.token";

// Minimal token holder — the full OAuth/login flow lands in epic 25.
// Persisted to localStorage so a reload keeps the session.
export const useAuthStore = defineStore("auth", {
  state: () => ({
    token: localStorage.getItem(TOKEN_KEY) as string | null,
  }),
  getters: {
    isLoggedIn: (s) => s.token !== null,
  },
  actions: {
    setToken(token: string | null) {
      this.token = token;
      if (token) localStorage.setItem(TOKEN_KEY, token);
      else localStorage.removeItem(TOKEN_KEY);
    },
  },
});
