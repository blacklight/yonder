import { describe, expect, it, vi } from "vitest";
import { nextTick } from "vue";

// `resetOnUserChange` keeps module-level registration state, so each
// test re-imports the modules against the current (fresh) pinia.
async function fresh() {
  vi.resetModules();
  const [{ resetOnUserChange }, { useAuthStore }] = await Promise.all([
    import("./perUser"),
    import("./auth"),
  ]);
  return { resetOnUserChange, useAuthStore };
}

describe("resetOnUserChange", () => {
  it("fires when the token is set, changed and cleared", async () => {
    const { resetOnUserChange, useAuthStore } = await fresh();
    const spy = vi.fn();
    resetOnUserChange(spy);
    const auth = useAuthStore();

    auth.setToken("a");
    await nextTick();
    auth.setToken("b");
    await nextTick();
    auth.setToken(null);
    await nextTick();
    expect(spy).toHaveBeenCalledTimes(3);
  });

  it("does not fire when the token is unchanged", async () => {
    const { resetOnUserChange, useAuthStore } = await fresh();
    const spy = vi.fn();
    resetOnUserChange(spy);
    const auth = useAuthStore();
    auth.setToken("same");
    await nextTick();
    auth.setToken("same");
    await nextTick();
    expect(spy).toHaveBeenCalledTimes(1);
  });
});
