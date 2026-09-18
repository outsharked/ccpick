// ccpick web portal. No frameworks, no build step: this file is served byte-for-byte from the
// binary, so it has to work standalone in any browser that supports EventSource and <dialog>.
(() => {
  "use strict";

  // Matches the TUI's own `DEBOUNCE` constant (src/ui/mod.rs) so full-text search feels the
  // same from either interface.
  const DEBOUNCE_MS = 150;
  const TOKEN_HEADER = "X-CCPick-Token";

  // The token arrives once, in the URL the server printed. It never has anywhere else to live
  // (there is no login), so it's kept in memory and sent on every request from here on, and the
  // URL is scrubbed immediately so it doesn't linger in history or get shared by accident.
  const TOKEN = new URLSearchParams(location.search).get("t") || "";
  history.replaceState({}, "", "/");

  const els = {
    search: document.getElementById("search"),
    counts: document.getElementById("counts"),
    list: document.getElementById("session-list"),
    preview: document.getElementById("preview"),
    status: document.getElementById("statusline"),
    dialog: document.getElementById("command-dialog"),
    dialogMessage: document.getElementById("dialog-message"),
    dialogCommand: document.getElementById("dialog-command"),
    dialogNote: document.getElementById("dialog-note"),
    dialogFocus: document.getElementById("dialog-focus"),
    dialogCopy: document.getElementById("dialog-copy"),
    dialogClose: document.getElementById("dialog-close"),
  };

  const state = {
    sessions: [],
    byId: new Map(),
    warnings: [],
    query: "",
    order: [], // session ids, ranked by /api/search's `matches`
    hits: [], // [{id, snippet}] from /api/search
    selectedId: null,
  };

  // Every response carrying a body this stale would be worse than showing nothing: a slow
  // `/api/search` or `/api/messages` reply for a query or row the user has already moved past.
  let searchSeq = 0;
  let previewSeq = 0;
  let debounceHandle = null;
  let statusTimer = null;
  let dialogFocusId = null;

  async function api(path, opts) {
    const headers = Object.assign({}, (opts && opts.headers) || {});
    headers[TOKEN_HEADER] = TOKEN;
    return fetch(path, Object.assign({}, opts, { headers }));
  }

  async function getJSON(path) {
    const res = await api(path);
    if (!res.ok) {
      throw new Error(`${path} -> ${res.status}`);
    }
    return res.json();
  }

  async function postJSON(path, payload) {
    const res = await api(path, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(payload),
    });
    let body = null;
    try {
      body = await res.json();
    } catch {
      // A body-less response (shouldn't happen here, but never let a parse failure hide the
      // status code from the caller).
    }
    return { status: res.status, body };
  }

  function escapeHtml(text) {
    return text.replace(
      /[&<>"']/g,
      (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c],
    );
  }

  // Wraps every case-insensitive occurrence of the query in <mark>, mirroring the TUI's own
  // `highlight_spans`. Matching runs on the raw text so byte offsets stay simple; escaping
  // happens per-segment afterwards so the query itself can't inject markup.
  function highlightHtml(text, query) {
    const needle = query.trim();
    if (!needle) return escapeHtml(text);
    const lower = text.toLowerCase();
    const needleLower = needle.toLowerCase();
    let out = "";
    let pos = 0;
    let at;
    while ((at = lower.indexOf(needleLower, pos)) !== -1) {
      out += escapeHtml(text.slice(pos, at));
      out += "<mark>" + escapeHtml(text.slice(at, at + needle.length)) + "</mark>";
      pos = at + needle.length;
    }
    return out + escapeHtml(text.slice(pos));
  }

  function setStatus(message, opts) {
    clearTimeout(statusTimer);
    els.status.textContent = message || "";
    if (message && !(opts && opts.persist)) {
      statusTimer = setTimeout(renderWarnings, 4000);
    }
  }

  function renderWarnings() {
    clearTimeout(statusTimer);
    els.status.textContent = state.warnings.join(" · ");
  }

  function updateCounts() {
    const running = state.sessions.filter((s) => s.running).length;
    els.counts.textContent = `${state.sessions.length} sessions · ${running} running`;
  }

  // The rows to draw: matches in ranked order, then (if there's a divider's worth of them) the
  // full-text hits below a divider — the same two-tier shape as the TUI's own row list.
  function computeRows() {
    const rows = [];
    for (const id of state.order) {
      const session = state.byId.get(id);
      if (session) rows.push({ session, snippet: null });
    }
    if (state.hits.length) {
      rows.push({ divider: true });
      for (const hit of state.hits) {
        const session = state.byId.get(hit.id);
        if (session) rows.push({ session, snippet: hit.snippet });
      }
    }
    return rows;
  }

  function rowDetail(session, snippet, query) {
    const detail = document.createElement("div");
    detail.className = "detail";
    if (snippet !== null) {
      detail.innerHTML = highlightHtml(snippet, query);
      return detail;
    }
    detail.textContent = [session.cwd || "?", session.source, session.when]
      .filter(Boolean)
      .join(" · ");
    if (session.running) {
      const tag = document.createElement("span");
      tag.className = "running-tag";
      tag.textContent = " [running]";
      detail.appendChild(tag);
    }
    return detail;
  }

  function renderList() {
    const rows = computeRows();
    const visible = rows.filter((r) => r.session).map((r) => r.session.id);
    const previousSelected = state.selectedId;
    if (state.selectedId === null || !visible.includes(state.selectedId)) {
      state.selectedId = visible[0] || null;
    }

    els.list.innerHTML = "";
    for (const row of rows) {
      if (row.divider) {
        const li = document.createElement("li");
        li.className = "divider";
        li.textContent = "── in conversation text ──";
        els.list.appendChild(li);
        continue;
      }
      const { session, snippet } = row;
      const li = document.createElement("li");
      li.className = "row";
      li.dataset.id = session.id;
      li.setAttribute("role", "option");

      const titleLine = document.createElement("div");
      titleLine.className = "title-line";
      const marker = document.createElement("span");
      marker.className = "marker";
      marker.setAttribute("aria-hidden", "true");
      marker.textContent = session.running ? "●" : "";
      const title = document.createElement("span");
      title.className = "title";
      title.innerHTML = highlightHtml(session.title, state.query);
      titleLine.append(marker, title);

      li.append(titleLine, rowDetail(session, snippet, state.query));
      li.addEventListener("click", () => onRowClick(session.id));
      els.list.appendChild(li);
    }

    updateSelectionClasses();
    updateCounts();
    if (state.selectedId !== previousSelected) {
      if (state.selectedId) {
        loadPreview(state.selectedId);
      } else {
        showEmptyPreview();
      }
    }
  }

  // Only touches classes/scroll, never rebuilds the list — so moving the selection with the
  // arrow keys doesn't reset the list's scroll position the way a full re-render would.
  function updateSelectionClasses() {
    for (const li of els.list.querySelectorAll(".row")) {
      const selected = li.dataset.id === state.selectedId;
      li.classList.toggle("selected", selected);
      if (selected) li.scrollIntoView({ block: "nearest" });
    }
  }

  function visibleIds() {
    return Array.from(els.list.querySelectorAll(".row")).map((li) => li.dataset.id);
  }

  function moveSelection(delta) {
    const ids = visibleIds();
    if (!ids.length) return;
    const at = ids.indexOf(state.selectedId);
    const next = at === -1 ? 0 : Math.max(0, Math.min(ids.length - 1, at + delta));
    selectRow(ids[next]);
  }

  function selectRow(id) {
    if (state.selectedId === id) return;
    state.selectedId = id;
    updateSelectionClasses();
    loadPreview(id);
  }

  function onRowClick(id) {
    if (state.selectedId === id) {
      activate(id);
    } else {
      selectRow(id);
    }
  }

  function showEmptyPreview() {
    els.preview.innerHTML = '<p class="empty">no sessions match</p>';
  }

  function renderPreview(session, messages) {
    els.preview.innerHTML = "";
    const cwdLine = document.createElement("div");
    cwdLine.className = "header-line";
    cwdLine.textContent = `${session.cwd || "?"} · ${session.branch || "-"} · ${session.source}`;
    const metaLine = document.createElement("div");
    metaLine.className = "header-line";
    metaLine.textContent = `${session.messages} msgs · last ${session.exact}`;
    const idLine = document.createElement("div");
    idLine.className = "header-id";
    idLine.textContent = session.id;
    const rule = document.createElement("hr");
    rule.className = "header-rule";
    els.preview.append(cwdLine, metaLine, idLine, rule);

    if (!messages.length) {
      const empty = document.createElement("p");
      empty.className = "empty";
      empty.textContent = "no messages";
      els.preview.appendChild(empty);
      return;
    }
    for (const m of messages) {
      const div = document.createElement("div");
      div.className = `message ${m.role}`;
      const role = document.createElement("span");
      role.className = "role";
      role.textContent = m.role === "user" ? "you: " : "assistant: ";
      div.append(role, document.createTextNode(m.text));
      els.preview.appendChild(div);
    }
  }

  async function loadPreview(id) {
    const mine = ++previewSeq;
    const session = state.byId.get(id);
    if (!session) {
      showEmptyPreview();
      return;
    }
    try {
      const data = await getJSON(`/api/messages?id=${encodeURIComponent(id)}`);
      if (mine !== previewSeq) return;
      renderPreview(session, data.messages);
    } catch {
      if (mine !== previewSeq) return;
      els.preview.innerHTML = '<p class="empty">could not load this conversation</p>';
    }
  }

  async function runSearch(query) {
    const mine = ++searchSeq;
    try {
      const data = await getJSON(`/api/search?q=${encodeURIComponent(query)}`);
      if (mine !== searchSeq) return;
      state.order = data.matches || [];
      state.hits = data.hits || [];
      renderList();
    } catch {
      if (mine !== searchSeq) return;
      setStatus("search failed — is ccpick still running?");
    }
  }

  async function loadSessions() {
    try {
      const data = await getJSON("/api/sessions");
      state.sessions = data.sessions || [];
      state.byId = new Map(state.sessions.map((s) => [s.id, s]));
      state.warnings = data.warnings || [];
      renderWarnings();
      await runSearch(state.query);
    } catch {
      setStatus("could not reach ccpick", { persist: true });
    }
  }

  function subscribeEvents() {
    const source = new EventSource(`/api/events?t=${encodeURIComponent(TOKEN)}`);
    source.addEventListener("generation", loadSessions);
  }

  function openDialog(session, body) {
    const reason = body.error ? `${body.error}. ` : "";
    els.dialogMessage.textContent = `${reason}Paste this into ${body.shell || "your shell"}:`;
    els.dialogCommand.textContent = body.command;
    if (body.pid) {
      els.dialogNote.hidden = false;
      els.dialogNote.textContent = `Still running as pid ${body.pid}.`;
      els.dialogFocus.hidden = false;
      dialogFocusId = session.id;
    } else {
      els.dialogNote.hidden = true;
      els.dialogFocus.hidden = true;
      dialogFocusId = null;
    }
    els.dialogCopy.textContent = "Copy command";
    els.dialog.showModal();
  }

  async function activate(id) {
    const session = state.byId.get(id);
    if (!session) return;
    const endpoint = session.running ? "/api/focus" : "/api/launch";
    const { status, body } = await postJSON(endpoint, { id });
    if (status === 200) {
      setStatus(session.running ? `focused ${session.title}` : `launching ${session.title}`);
    } else if (status === 409 && body && body.command) {
      openDialog(session, body);
    } else {
      setStatus((body && body.error) || `could not reach ${session.title}`);
    }
  }

  els.dialogCopy.addEventListener("click", async () => {
    const text = els.dialogCommand.textContent;
    try {
      await navigator.clipboard.writeText(text);
      els.dialogCopy.textContent = "Copied";
    } catch {
      // No async clipboard access (e.g. an insecure context) — fall back to a selection the
      // user can copy themselves with the keyboard.
      const range = document.createRange();
      range.selectNodeContents(els.dialogCommand);
      const selection = window.getSelection();
      selection.removeAllRanges();
      selection.addRange(range);
      els.dialogCopy.textContent = "Selected — copy with your keyboard";
    }
    setTimeout(() => {
      els.dialogCopy.textContent = "Copy command";
    }, 1500);
  });

  els.dialogFocus.addEventListener("click", async () => {
    const id = dialogFocusId;
    els.dialog.close();
    if (!id) return;
    const { status, body } = await postJSON("/api/focus", { id });
    setStatus(status === 200 ? "focused" : (body && body.error) || "could not focus that session");
  });

  els.dialogClose.addEventListener("click", () => els.dialog.close());

  els.search.addEventListener("input", () => {
    state.query = els.search.value;
    clearTimeout(debounceHandle);
    debounceHandle = setTimeout(() => runSearch(state.query), DEBOUNCE_MS);
  });

  document.addEventListener("keydown", (event) => {
    if (els.dialog.open) {
      if (event.key === "Escape") els.dialog.close();
      return;
    }
    if (event.key === "/" && document.activeElement !== els.search) {
      event.preventDefault();
      els.search.focus();
      els.search.select();
      return;
    }
    if (event.key === "ArrowDown") {
      event.preventDefault();
      moveSelection(1);
      return;
    }
    if (event.key === "ArrowUp") {
      event.preventDefault();
      moveSelection(-1);
      return;
    }
    if (event.key === "Enter" && state.selectedId) {
      event.preventDefault();
      activate(state.selectedId);
    }
  });

  showEmptyPreview();
  loadSessions();
  subscribeEvents();
})();
