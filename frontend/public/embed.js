/*
 * Yonder embed script.
 *
 * Renders ``<div class="yonder-embed" data-status="<id>"></div>``
 * placeholders as the standalone embed page inside an iframe, then
 * keeps the iframe height in sync with the embedded post via
 * postMessage.
 *
 * Usage:
 *   <div class="yonder-embed" data-status="<id>"></div>
 *   <script async src="https://<instance>/embed.js" charset="utf-8"></script>
 */
(function () {
  "use strict";

  // Keep in sync with EMBED_IFRAME_HEIGHT in src/utils/embed.ts.
  var DEFAULT_HEIGHT = 400;

  var scriptEl = document.currentScript;
  if (!scriptEl) {
    var scripts = document.getElementsByTagName("script");
    for (var i = scripts.length - 1; i >= 0; i--) {
      if (/\/embed\.js(?:[?#]|$)/.test(scripts[i].src || "")) {
        scriptEl = scripts[i];
        break;
      }
    }
  }

  var origin = "";
  try {
    origin = scriptEl ? new URL(scriptEl.src, window.location.href).origin : "";
  } catch {
    origin = "";
  }
  if (!origin) return;

  var rendered = [];

  function renderPlaceholder(el) {
    if (el.getAttribute("data-yonder-rendered")) return;
    var id = el.getAttribute("data-status");
    if (!id || !/^[\w.-]+$/.test(id)) return;
    el.setAttribute("data-yonder-rendered", "1");

    // The placeholder may carry a fallback <a> (no-JS link + the
    // Webmention backlink) — replace it once the live frame exists.
    el.textContent = "";
    var iframe = document.createElement("iframe");
    iframe.src = origin + "/statuses/" + encodeURIComponent(id) + "/embed";
    iframe.width = "100%";
    iframe.height = String(DEFAULT_HEIGHT);
    iframe.setAttribute("frameborder", "0");
    iframe.setAttribute("loading", "lazy");
    iframe.setAttribute("scrolling", "no");
    iframe.style.width = "100%";
    iframe.style.border = "0";
    iframe.style.overflow = "hidden";
    iframe.style.display = "block";
    el.appendChild(iframe);
    rendered.push(iframe);
  }

  function renderAll(root) {
    var scope = root || document;
    var nodes = scope.querySelectorAll(".yonder-embed, [data-yonder-embed]");
    for (var i = 0; i < nodes.length; i++) {
      renderPlaceholder(nodes[i]);
    }
  }

  window.addEventListener("message", function (event) {
    if (event.origin !== origin) return;
    var data = event.data;
    if (
      !data ||
      data.source !== "yonder-embed" ||
      data.type !== "resize" ||
      typeof data.height !== "number"
    ) {
      return;
    }
    for (var i = 0; i < rendered.length; i++) {
      if (rendered[i].contentWindow === event.source) {
        rendered[i].style.height = Math.max(0, data.height) + "px";
        rendered[i].height = String(Math.max(0, data.height));
      }
    }
  });

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", function () {
      renderAll();
    });
  } else {
    renderAll();
  }
})();
