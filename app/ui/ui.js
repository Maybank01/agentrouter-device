// Shared by the app's local pages: calls into the app (local commands only), polling, icons.
(function () {
  const internals = window.__TAURI_INTERNALS__;
  const call = (cmd, args) => (internals ? internals.invoke(cmd, args || {}) : Promise.reject(new Error("no app")));
  const act = (action, arg) => call("ui_action", { action, arg: arg === undefined ? null : arg });

  // Poll the app state; `render` gets the newest state, never two calls at once.
  function poll(render, ms) {
    let busy = false;
    const tick = async () => {
      if (busy) return;
      busy = true;
      try { render(await call("ui_state")); } catch (e) { /* the app is closing */ }
      busy = false;
    };
    tick();
    return setInterval(tick, ms || 500);
  }

  // Size the native window to the card (plus the 12px shadow margin on each side).
  let lastFit = "";
  function fit(el, extra) {
    const box = (el || document.querySelector(".win") || document.body).getBoundingClientRect();
    const m = extra === undefined ? 24 : extra;
    const w = Math.ceil(box.width + m), h = Math.ceil(box.height + m);
    const key = w + "x" + h;
    if (key === lastFit) return;
    lastFit = key;
    call("win_fit", { width: w, height: h }).catch(() => {});
  }

  const esc = (s) => String(s == null ? "" : s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

  function timer(ms) {
    const s = Math.max(0, Math.floor(ms / 1000));
    const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60), r = s % 60;
    const two = (n) => String(n).padStart(2, "0");
    return h ? `${h}:${two(m)}:${two(r)}` : `${two(m)}:${two(r)}`;
  }

  function clock(ms) {
    const d = new Date(ms);
    return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
  }

  async function copy(text) {
    try { await navigator.clipboard.writeText(text); return true; } catch (e) { /* fall back */ }
    const t = document.createElement("textarea");
    t.value = text; t.style.position = "fixed"; t.style.opacity = "0";
    document.body.appendChild(t); t.select();
    let ok = false;
    try { ok = document.execCommand("copy"); } catch (e) { ok = false; }
    t.remove();
    return ok;
  }

  // Line icons (24px grid).
  const P = {
    pc: '<rect x="3" y="4" width="18" height="12" rx="2"/><path d="M8 20h8M12 16v4"/>',
    laptop: '<rect x="5" y="5" width="14" height="10" rx="1.5"/><path d="M3 19h18"/>',
    power: '<path d="M12 3v9"/><path d="M6.3 7.3a8 8 0 1 0 11.4 0"/>',
    share: '<path d="M12 15V3M7 8l5-5 5 5"/><path d="M5 13v6a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2v-6"/>',
    link: '<path d="M10 14a4.5 4.5 0 0 0 6.4 0l3-3a4.5 4.5 0 0 0-6.4-6.4l-1 1"/><path d="M14 10a4.5 4.5 0 0 0-6.4 0l-3 3a4.5 4.5 0 0 0 6.4 6.4l1-1"/>',
    folder: '<path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/>',
    activity: '<path d="M3 12h4l3-7 4 14 3-7h4"/>',
    stop: '<rect x="7" y="7" width="10" height="10" rx="1.5" fill="currentColor"/>',
    x: '<path d="M6 6l12 12M18 6L6 18"/>',
    copy: '<rect x="8" y="8" width="12" height="12" rx="2"/><path d="M16 8V6a2 2 0 0 0-2-2H6a2 2 0 0 0-2 2v8a2 2 0 0 0 2 2h2"/>',
    refresh: '<path d="M20 12a8 8 0 1 1-2.3-5.7L20 8"/><path d="M20 3v5h-5"/>',
    shield: '<path d="M12 3l8 3v6c0 4.5-3.4 8-8 9-4.6-1-8-4.5-8-9V6z"/><path d="M9 12l2 2 4-4"/>',
    term: '<rect x="3" y="4" width="18" height="16" rx="2.5"/><path d="M7 9l3 3-3 3M13 15h4"/>',
    pause: '<path d="M9 6v12M15 6v12"/>',
    chev: '<path d="M9 6l6 6-6 6"/>',
    chevl: '<path d="M15 6l-6 6 6 6"/>',
    check: '<path d="M5 12l5 5 9-10"/>',
    plus: '<path d="M12 5v14M5 12h14"/>',
    warn: '<path d="M12 4l9 16H3z"/><path d="M12 10v4M12 17v.5"/>',
    eye: '<path d="M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7S2 12 2 12z"/><circle cx="12" cy="12" r="3"/>',
    ask: '<path d="M4 5h16v11H9l-5 4z"/><path d="M9 10h6"/>',
    key: '<circle cx="8" cy="15" r="4"/><path d="M11 12l9-9M16 7l3 3"/>',
    chat: '<path d="M4 5h16v11H9l-5 4z"/>',
    sparkle: '<path d="M12 3v4M12 17v4M3 12h4M17 12h4M6 6l2.5 2.5M15.5 15.5L18 18M6 18l2.5-2.5M15.5 8.5L18 6"/>',
    grip: '<circle cx="9" cy="6" r="1.2" fill="currentColor"/><circle cx="15" cy="6" r="1.2" fill="currentColor"/><circle cx="9" cy="12" r="1.2" fill="currentColor"/><circle cx="15" cy="12" r="1.2" fill="currentColor"/><circle cx="9" cy="18" r="1.2" fill="currentColor"/><circle cx="15" cy="18" r="1.2" fill="currentColor"/>',
    external: '<path d="M14 4h6v6M20 4l-9 9"/><path d="M18 14v5a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1h5"/>',
  };
  const icon = (name, cls) => `<svg class="i ${cls || ""}" viewBox="0 0 24 24" aria-hidden="true">${P[name] || ""}</svg>`;

  const LEVELS = {
    readonly: { icon: "eye", label: "只读", tag: "最稳妥", text: "只看文件、列目录，什么都不改。" },
    folders: { icon: "folder", label: "只限这些文件夹", text: "只动你选的文件夹，跑命令前问你。" },
    confirm: { icon: "ask", label: "每条都确认", text: "每跑一条命令、每改一个文件，都先问你。" },
    full: { icon: "key", label: "完全访问", text: "和你自己坐在电脑前一样。" },
  };

  // Title-bar buttons for frameless windows.
  const WC_MIN = '<button class="wc" data-wc="min" title="最小化"><svg viewBox="0 0 10 10"><path d="M1 5h8"/></svg></button>';
  const WC_X = '<button class="wc x" data-wc="close" title="关闭"><svg viewBox="0 0 10 10"><path d="M1 1l8 8M9 1l-8 8"/></svg></button>';
  function wireTitlebar(root) {
    root.addEventListener("mousedown", (e) => {
      const tb = e.target.closest(".tb");
      if (tb && !e.target.closest("button") && e.button === 0) call("win_drag").catch(() => {});
    });
    root.addEventListener("click", (e) => {
      const b = e.target.closest("[data-wc]");
      if (!b) return;
      call(b.dataset.wc === "min" ? "win_minimize" : "win_close").catch(() => {});
    });
  }

  // No browser context menu or text-selection look in the app's own windows.
  document.addEventListener("contextmenu", (e) => { if (!e.target.closest("input, textarea, .codebox")) e.preventDefault(); });

  window.AR = { call, act, poll, fit, esc, timer, clock, copy, icon, LEVELS, WC_MIN, WC_X, wireTitlebar };
})();
