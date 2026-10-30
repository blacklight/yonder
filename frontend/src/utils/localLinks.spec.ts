import { afterEach, describe, expect, it, vi } from "vitest";
import { localLinkNavHandler } from "./localLinks";

function mount(html: string) {
  document.body.innerHTML = html;
  return document.querySelector("a")!;
}

function clickOn(el: Element, init: MouseEventInit = {}) {
  const e = new MouseEvent("click", {
    bubbles: true,
    cancelable: true,
    button: 0,
    ...init,
  });
  el.dispatchEvent(e);
  return e;
}

describe("localLinkNavHandler", () => {
  afterEach(() => {
    document.body.innerHTML = "";
    vi.restoreAllMocks();
  });

  function setup() {
    const push = vi.fn();
    const handler = localLinkNavHandler({ push });
    document.addEventListener("click", handler);
    return { push, done: () => document.removeEventListener("click", handler) };
  }

  it("routes local-path anchors through the router", () => {
    const { push, done } = setup();
    const e = clickOn(mount(`<a href="/tags/rust">#rust</a>`));
    expect(e.defaultPrevented).toBe(true);
    expect(push).toHaveBeenCalledWith("/tags/rust");
    done();
  });

  // jsdom can't navigate — swallow the default action after our
  // handler (registered first) has had its say.
  function suppressNav() {
    const f = (e: Event) => e.preventDefault();
    document.addEventListener("click", f);
    return () => document.removeEventListener("click", f);
  }

  it("ignores remote links, downloads and _blank targets", () => {
    const { push, done } = setup();
    const quiet = suppressNav();
    document.body.innerHTML =
      `<a href="https://x.test/@a" id="r">x</a>` +
      `<a href="/media/a.png" id="d" download>x</a>` +
      `<a href="/x" id="b" target="_blank">x</a>`;
    for (const id of ["r", "d", "b"]) {
      clickOn(document.getElementById(id)!);
    }
    expect(push).not.toHaveBeenCalled();
    quiet();
    done();
  });

  it("leaves modified clicks for the browser", () => {
    const { push, done } = setup();
    const quiet = suppressNav();
    const a = mount(`<a href="/@alice">@a</a>`);
    for (const init of [
      { metaKey: true },
      { ctrlKey: true },
      { shiftKey: true },
      { button: 1 },
    ]) {
      clickOn(a, init);
    }
    expect(push).not.toHaveBeenCalled();
    quiet();
    done();
  });

  it("ignores clicks already handled and non-anchor clicks", () => {
    const { push, done } = setup();
    const a = mount(`<a href="/x">x</a><button>b</button>`);
    // router-link prevents default before the click reaches document.
    a.addEventListener("click", (e) => e.preventDefault());
    clickOn(a);
    expect(push).not.toHaveBeenCalled();
    clickOn(document.querySelector("button")!);
    expect(push).not.toHaveBeenCalled();
    done();
  });
});
