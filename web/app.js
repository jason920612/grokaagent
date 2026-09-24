(() => {
  const params = new URLSearchParams(location.search);
  const token = params.get("t") || "";
  const $ = (id) => document.getElementById(id);
  const appEl = $("app");
  const chat = $("chat");
  const composer = $("composer");
  const overlay = $("overlay");

  // —— Server state (mirrored from the TUI) ——
  let snap = null;          // shell: header, composer, sessions, overlays
  let logs = { agents: [], tools: [], events: [], rail: { monitors: [], backgrounds: [] }, changes: [] };
  const views = new Map();  // "" = main chat, "coder/lint" = child agent
  let seq = 0;

  // —— Local UI state (per browser tab) ——
  const narrow = window.matchMedia("(max-width: 900px)");
  let sideView = "sessions";
  let sideOpen = !narrow.matches;
  let panelTab = null;      // null | "tools" | "output" | "events"
  let outputPick = null;
  const openTabs = [];      // agent paths
  let activeTab = "";       // "" = main chat
  let stick = true;
  const scrollPositions = new Map();
  // Deep link: #side=agents&tab=coder&panel=tools
  const linked = new URLSearchParams(location.hash.slice(1));
  let pendingTab = linked.get("tab") || "";
  if (["sessions", "agents", "changes", "background", "task"].includes(linked.get("side"))) {
    sideView = linked.get("side");
    sideOpen = true;
  }
  if (["tools", "output", "events"].includes(linked.get("panel"))) panelTab = linked.get("panel");

  let ws;
  let connected = false;
  let composing = false;
  let settingsFocus = null;
  let sendMode = "queue";
  let draftSession = "";
  let awaitingReceipt = null;
  let receiptRetryReady = false;
  const drafts = new Map();
  const folds = new Map();
  let overlayKey = "";
  let overlayComposing = false;
  let localView = {};
  let taskDraft = "";

  const STATE_ICON = { starting: "◌", working: "◐", idle: "✓", paused: "‖", interrupted: "⊘", exited: "○" };

  function el(tag, cls, text) {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text !== undefined) e.textContent = text;
    return e;
  }

  function agentOf(path) {
    return logs.agents.find((a) => a.path === path);
  }

  function viewRows(path) {
    return views.get(path) || [];
  }

  function connectionState(ok, message) {
    connected = ok;
    const c = $("connection");
    c.textContent = message;
    c.classList.toggle("offline", !ok);
    c.classList.toggle("ok", ok);
    $("statusbar").classList.toggle("offline", !ok);
    $("send-message").disabled = !ok || (!!awaitingReceipt && !receiptRetryReady);
  }

  function submitMessage(forceInsert = false) {
    if (!connected || !snap) {
      $("delivery").textContent = "未送出：連線中斷，草稿已保留";
      return;
    }
    if (awaitingReceipt) {
      send(awaitingReceipt);
      $("delivery").textContent = "重新確認同一則訊息，不會重複加入";
      return;
    }
    if (!composer.value.trim() && !snap.pending.length) return;
    const request = { type: "submit_text", session_id: snap.session_id,
      request_id: crypto.randomUUID(), text: composer.value, insert: forceInsert || sendMode === "insert" };
    awaitingReceipt = request;
    receiptRetryReady = false;
    $("send-message").disabled = true;
    $("delivery").textContent = "送出中，等待接收確認…";
    send(request);
    setTimeout(() => {
      if (awaitingReceipt?.request_id === request.request_id) {
        receiptRetryReady = true;
        $("delivery").textContent = "尚未收到確認，草稿保留；可點送出重新確認";
        $("send-message").disabled = !connected;
      }
    }, 8000);
  }

  function mediaUrl(path) {
    return `/media?t=${encodeURIComponent(token)}&path=${encodeURIComponent(path)}`;
  }

  function wsSend(obj) {
    if (ws && ws.readyState === 1) { ws.send(JSON.stringify(obj)); return true; }
    return false;
  }

  function send(obj) {
    if (obj.type === "open_settings") {
      localView = { settings: true }; renderOverlay();
      return send({ type: "refresh_settings" });
    }
    if (obj.type === "open_task") { taskDraft = ""; localView = { task: true }; renderOverlay(); return true; }
    if (obj.type === "set_task_draft") { taskDraft = obj.text; return true; }
    if (obj.type === "submit_task") {
      if (!taskDraft.trim()) return false;
      const ok = send({ type: "start_task", session_id: snap.session_id, goal: taskDraft });
      if (ok) { localView = {}; renderOverlay(); }
      return ok;
    }
    if (obj.type === "begin_rename") {
      localView = { rename: { id: obj.id, text: snap.sessions.find(s => s.id === obj.id)?.name || "" } };
      renderOverlay(); return true;
    }
    if (obj.type === "commit_rename") {
      const ok = send({ type: "rename", id: localView.rename.id, text: obj.text });
      if (ok) { localView = {}; renderOverlay(); } return ok;
    }
    if (["close_settings", "close_task", "cancel_rename"].includes(obj.type)) {
      localView = {}; renderOverlay(); return true;
    }
    // Reading details is local to this browser, independent of the terminal.
    const local = {
      open_tool: () => ({ tool_panel: { view: obj.view || "", group: obj.group, item: obj.item } }),
      open_image: () => ({ image_view: obj.path }),
      open_monitor: () => ({ inspector: { kind: "monitor", name: obj.name } }),
      open_agent_info: () => ({ agent_info: obj.path }),
      edit_queue: () => ({ queue_editor: { index: obj.index, original: snap.queue[obj.index].text } }),
    };
    if (local[obj.type]) { localView = local[obj.type](); renderOverlay(); return true; }
    if (["close_tool", "close_image", "close_inspector", "close_queue_editor", "close_agent_info"].includes(obj.type)) {
      localView = {}; renderOverlay(); return true;
    }
    if (connected && wsSend(obj)) return true;
    $("delivery").textContent = "未送出：連線中斷，請重連後再操作";
    return false;
  }

  function providerKind() {
    const fromHeader = snap && snap.header && snap.header.kind;
    const fromSettings = snap && snap.settings_data && snap.settings_data.kind;
    return String(fromSettings || fromHeader || "xai").toLowerCase();
  }

  function isCustom() {
    return providerKind() === "openai";
  }

  function agentLabel() {
    if (activeTab) return activeTab.split("/").pop();
    if (isCustom()) return "助手";
    const model = (snap && snap.header && snap.header.model) || "";
    return model.toLowerCase().startsWith("grok") ? "grok" : "助手";
  }

  // —— Protocol ——
  function onHello(msg) {
    seq = msg.seq;
    logs = msg.logs || logs;
    views.clear();
    (msg.views || []).forEach((v) => views.set(v.path, v.rows));
    applyShell(msg.snapshot);
    if (pendingTab && agentOf(pendingTab)) {
      openTabs.push(pendingTab);
      activeTab = pendingTab;
    }
    pendingTab = "";
    renderAll();
  }

  function onDelta(msg) {
    if (msg.seq <= seq) return;
    if (msg.seq !== seq + 1) {
      wsSend({ type: "resync" });
      return;
    }
    seq = msg.seq;
    if (msg.logs) logs = msg.logs;
    (msg.views || []).forEach((p) => {
      const rows = views.get(p.path) || [];
      rows.length = Math.min(p.from, rows.length);
      rows.push(...p.rows);
      rows.length = p.len;
      views.set(p.path, rows);
    });
    (msg.removed || []).forEach((path) => views.delete(path));
    if (msg.snapshot) applyShell(msg.snapshot);
    renderAll();
  }

  function applyShell(s) {
    if (draftSession !== s.session_id) {
      drafts.set(draftSession, composer.value);
      draftSession = s.session_id;
      composer.value = drafts.get(draftSession) || "";
      localView = {};
      // Agent tabs belong to a session.
      openTabs.length = 0;
      activeTab = "";
    }
    snap = s;
    if (awaitingReceipt) {
      const receipt = (s.receipts || []).find(([id]) => id === awaitingReceipt.request_id);
      if (receipt) {
        const request = awaitingReceipt;
        if (receipt[1] === "已接收") {
          if (request.type === "update_queue") {
            localView = {};
            $("delivery").textContent = "已儲存待處理訊息";
          } else {
            if (s.session_id === request.session_id && composer.value === request.text) composer.value = "";
            if (drafts.get(request.session_id) === request.text) drafts.set(request.session_id, "");
            $("delivery").textContent = request.insert ? "已接收 · 下一輪處理調整" : "已接收 · 依序處理";
          }
        } else {
          $("delivery").textContent = receipt[1];
          const notice = overlay.querySelector(".queue-notice");
          if (notice) notice.textContent = receipt[1];
        }
        awaitingReceipt = null;
        receiptRetryReady = false;
        $("send-message").disabled = !connected;
      }
    }
  }

  function renderAll() {
    if (!snap) return;
    // Drop tabs whose agent is gone (session switched, or never existed here).
    for (let i = openTabs.length - 1; i >= 0; i--) {
      if (!agentOf(openTabs[i])) openTabs.splice(i, 1);
    }
    if (activeTab && !agentOf(activeTab)) activeTab = "";
    renderActivity();
    renderSide();
    renderTabs();
    renderChat();
    renderEditorFooter();
    renderPanel();
    renderStatus();
    renderOverlay();
  }

  // —— Activity bar & side bar ——
  function setSide(view) {
    if (view === undefined) {
      sideOpen = !sideOpen;
    } else if (sideOpen && sideView === view) {
      sideOpen = false;
    } else {
      sideView = view;
      sideOpen = true;
    }
    renderAll();
  }

  function closeSideIfNarrow() {
    if (narrow.matches) { sideOpen = false; renderAll(); }
  }

  function renderActivity() {
    appEl.classList.toggle("side-open", sideOpen);
    $("side-scrim").classList.toggle("hidden", !(sideOpen && narrow.matches));
    document.querySelectorAll(".act[data-view]").forEach((b) => {
      b.classList.toggle("on", sideOpen && b.dataset.view === sideView);
      const badge = b.querySelector(".badge");
      if (!badge) return;
      let n = "";
      if (b.dataset.view === "agents") {
        const live = logs.agents.filter((a) => a.state === "working" || a.state === "starting").length;
        n = live ? String(live) : "";
      } else if (b.dataset.view === "changes") {
        const files = new Set(logs.changes.map((c) => c.path)).size;
        n = files ? String(files) : "";
      } else if (b.dataset.view === "background") {
        const live = logs.rail.backgrounds.filter((x) => x.alive).length + logs.rail.monitors.filter((x) => x.alive).length;
        n = live ? String(live) : "";
      } else if (b.dataset.view === "task") {
        n = snap.header.task_live ? "●" : "";
      }
      badge.textContent = n;
      badge.classList.toggle("hidden", !n);
    });
  }

  let sideKey = "";
  function renderSide() {
    const titles = { sessions: "對話", agents: "代理", changes: "檔案變更", background: "背景工作", task: "任務" };
    const key = JSON.stringify([sideView, activeTab, snap.sessions, snap.header.status, snap.header.running,
      snap.header.model, !!snap.ask,
      sideView === "agents" ? logs.agents : null,
      sideView === "changes" ? logs.changes : null,
      sideView === "background" ? [logs.rail.monitors, logs.rail.backgrounds.map((b) => [b.name, b.status, b.alive, b.command]), outputPick] : null,
      sideView === "task" ? snap.task_summary : null]);
    if (key === sideKey) return;
    sideKey = key;
    const title = $("side-title");
    title.replaceChildren(el("span", "", titles[sideView]));
    const body = $("side-body");
    body.replaceChildren();
    if (sideView === "sessions") {
      const add = el("button", "", "＋ 新工作");
      add.type = "button";
      add.title = "新工作";
      add.addEventListener("click", () => { send({ type: "new_chat" }); closeSideIfNarrow(); });
      title.append(add);
      for (const s of snap.sessions || []) {
        const item = el("button", "side-item two session" + (s.current ? " on current" : ""));
        item.type = "button";
        const grow = el("div", "grow");
        grow.append(el("span", "name", s.name), el("span", "dim", `${s.status || "待命"} · ${s.folder}`));
        const acts = el("span", "acts");
        const ren = el("span", "", "✎");
        ren.title = "改名";
        ren.addEventListener("click", (e) => { e.stopPropagation(); send({ type: "begin_rename", id: s.id }); });
        const del = el("span", "", "✕");
        del.title = "刪除";
        del.addEventListener("click", (e) => { e.stopPropagation(); send({ type: "delete_session", id: s.id }); });
        acts.append(ren, del);
        item.append(el("span", "icon", s.current ? "●" : "○"), grow, acts);
        item.addEventListener("click", () => { send({ type: "switch", id: s.id }); closeSideIfNarrow(); });
        body.append(item);
      }
    } else if (sideView === "agents") {
      const root = el("button", "side-item two" + (activeTab === "" ? " on" : ""));
      root.type = "button";
      const rg = el("div", "grow");
      rg.append(el("span", "name", "主代理"), el("span", "dim", `${snap.ask ? "等你回覆" : snap.header.status} · ${snap.header.model}`));
      root.append(el("span", "icon" + (snap.header.running ? " state-working" : ""), snap.header.running ? "◐" : "●"), rg);
      root.addEventListener("click", () => { activate(""); closeSideIfNarrow(); });
      body.append(root);
      if (!logs.agents.length) {
        body.append(el("div", "side-note", "尚無子代理。主代理呼叫 spawn_agent 後，子代理與它們的子代理會以樹狀列在這裡；點選可開啟唯讀分頁查看完整工作紀錄。"));
      }
      for (const a of logs.agents) {
        const item = el("button", "side-item two" + (activeTab === a.path ? " on" : ""));
        item.type = "button";
        item.style.paddingLeft = `${16 + (a.depth + 1) * 14}px`;
        item.title = a.prompt || a.path;
        const grow = el("div", "grow");
        const turn = a.turn ? ` · 第${a.turn}輪` : "";
        grow.append(el("span", "name", a.name), el("span", "dim",
          `${a.label}${turn}${a.activity && (a.state === "working") ? " · " + a.activity : a.model ? " · " + a.model : ""}`));
        item.append(el("span", "icon state-" + a.state, STATE_ICON[a.state] || "·"), grow);
        item.addEventListener("click", () => { openAgent(a.path); closeSideIfNarrow(); });
        body.append(item);
      }
    } else if (sideView === "changes") {
      const files = new Set(logs.changes.map((c) => c.path));
      body.append(el("div", "side-note", `${files.size} 個檔案 · ${logs.changes.length} 次變更`));
      [...logs.changes].reverse().forEach((c) => {
        const mark = c.kind === "create" || c.kind === "add" ? "A" : c.kind === "delete" ? "D" : "M";
        const item = el("button", "side-item");
        item.type = "button";
        item.title = `${c.path}${c.view ? " · " + c.view : ""}`;
        item.append(el("span", "icon kind-" + mark, mark), el("span", "name", c.path));
        if (c.view) item.append(el("span", "dim", c.view));
        item.addEventListener("click", () => {
          send({ type: "open_tool", view: c.view, group: c.row, item: c.call });
          closeSideIfNarrow();
        });
        body.append(item);
      });
    } else if (sideView === "background") {
      const { backgrounds, monitors } = logs.rail;
      if (!backgrounds.length && !monitors.length) body.append(el("div", "side-note", "尚無背景行程、監控或計時器"));
      backgrounds.forEach((b) => {
        const item = el("button", "side-item two" + (outputPick === b.name ? " on" : ""));
        item.type = "button";
        const grow = el("div", "grow");
        grow.append(el("span", "name", `${b.name}  ${b.status}`), el("span", "dim", b.command));
        item.append(el("span", "icon " + (b.alive ? "state-working" : "state-exited"), b.alive ? "●" : "○"), grow);
        item.addEventListener("click", () => { outputPick = b.name; panelTab = "output"; renderAll(); closeSideIfNarrow(); });
        body.append(item);
      });
      monitors.forEach((m) => {
        const item = el("button", "side-item");
        item.type = "button";
        item.append(el("span", "icon " + (m.alive ? "state-paused" : "state-exited"), m.alive ? "●" : "○"),
          el("span", "name", `監控 ${m.name}`), el("span", "dim", m.status));
        item.addEventListener("click", () => send({ type: "open_monitor", name: m.name }));
        body.append(item);
      });
    } else if (sideView === "task") {
      const task = snap.task_summary || {};
      if (!task.goal) {
        body.append(el("div", "side-note", "尚未設定任務目標。任務模式會用監督者檢查表，讓主代理一直做到目標完成。"));
      } else {
        body.append(el("div", "side-note", `目標  ${task.goal}`));
        const done = (task.checklist || []).filter((i) => i.done).length;
        body.append(el("div", "side-section", `${task.phase} · ${done}/${(task.checklist || []).length}`));
        (task.checklist || []).forEach((i) => {
          body.append(el("div", "side-item check" + (i.done ? " done" : ""), `${i.done ? "✓" : "□"} ${i.text}`));
        });
        if (task.note) body.append(el("div", "side-note", task.note));
      }
      const btn = el("button", "side-btn", task.goal ? "開啟任務面板" : "設定任務目標");
      btn.type = "button";
      btn.addEventListener("click", () => send({ type: "open_task" }));
      body.append(btn);
    }
  }

  // —— Editor tabs ——
  function openAgent(path) {
    if (!openTabs.includes(path)) openTabs.push(path);
    activate(path);
  }

  function activate(path) {
    scrollPositions.set(activeTab, { top: chat.scrollTop, stick });
    activeTab = path;
    renderAll();
    if (!path) composer.focus();
  }

  function closeTab(path) {
    const i = openTabs.indexOf(path);
    if (i < 0) return;
    openTabs.splice(i, 1);
    if (activeTab === path) activeTab = openTabs[i - 1] ?? openTabs[0] ?? "";
    renderAll();
  }

  function cycleTab(delta) {
    const all = ["", ...openTabs];
    const i = all.indexOf(activeTab);
    activate(all[(i + delta + all.length) % all.length]);
  }

  let tabsKey = "";
  function renderTabs() {
    const key = JSON.stringify([activeTab, openTabs, snap.header.running,
      openTabs.map((p) => agentOf(p)?.state)]);
    if (key === tabsKey) return;
    tabsKey = key;
    const box = $("tabs");
    box.replaceChildren();
    const mk = (path, label, icon, stateCls) => {
      const t = el("div", "tab" + (activeTab === path ? " on" : ""));
      t.setAttribute("role", "tab");
      t.setAttribute("aria-selected", String(activeTab === path));
      t.tabIndex = 0;
      t.append(el("span", stateCls, icon), el("span", "", label));
      if (path) {
        const x = el("span", "close", "✕");
        x.title = "關閉 (Ctrl+W)";
        x.addEventListener("click", (e) => { e.stopPropagation(); closeTab(path); });
        t.append(x);
      }
      t.addEventListener("click", () => activate(path));
      t.addEventListener("auxclick", (e) => { if (e.button === 1 && path) closeTab(path); });
      box.append(t);
    };
    mk("", "主對話", snap.header.running ? "◐" : "●", snap.header.running ? "state-working" : "");
    openTabs.forEach((p) => {
      const a = agentOf(p);
      mk(p, a ? a.name : p, STATE_ICON[a?.state] || "·", "state-" + (a?.state || "exited"));
    });
  }

  // —— Chat ——
  function rowSig(row) {
    return JSON.stringify({
      kind: row.kind, html: row.html, text: row.text, expanded: row.expanded, done: row.done,
      images: row.images, calls: row.calls, path: row.path, label: row.label,
      work: row.kind === "agent" ? row.elapsed_ms : undefined,
    }) + "\0" + agentLabel();
  }

  function patchThinkClock(node, row) {
    if (row.kind !== "think") return;
    const sum = node.querySelector("summary");
    if (!sum) return;
    const next = row.done ? `思考 ${((row.elapsed_ms || 0) / 1000).toFixed(1)}s` : "思考中…";
    if (sum.textContent !== next) sum.textContent = next;
  }

  function buildChatRow(row, i, view) {
    const node = el("article", "row " + row.kind);
    node.dataset.sig = rowSig(row);
    node.append(el("div", "who",
      row.kind === "user" ? (view ? "上層代理" : "你") :
      row.kind === "agent" ? agentLabel() :
      row.kind === "think" ? "思考" :
      row.kind === "tools" ? "工具" :
      row.kind === "picture" ? "圖片" :
      row.kind === "err" ? "錯誤" : "系統"));

    if (row.kind === "think" || row.kind === "tools") {
      const d = el("details", "fold");
      const foldKey = `${snap.session_id}:${view}:${i}`;
      d.open = folds.get(foldKey) ?? !!row.expanded;
      const sum = el("summary", "", row.kind === "think"
        ? (row.done ? `思考 ${((row.elapsed_ms || 0) / 1000).toFixed(1)}s` : "思考中…")
        : `工具 ${row.calls.length}${row.calls.some(c => c.phase === "失敗") ? " · 有操作失敗" : row.calls.some(c => c.phase === "已停止") ? " · 已停止" : row.calls.some(c => !c.done) ? " · 執行中" : ""}`);
      d.append(sum);
      const body = el("div");
      body.innerHTML = row.html;
      if (row.kind === "tools") {
        row.calls.forEach((c, j) => {
          const call = el("div", "call");
          const mark = c.phase === "失敗" ? "!" : c.phase === "已停止" ? "⊘" : c.done ? "✓" : "▸";
          call.append(el("div", "head" + (c.phase === "失敗" ? " fail" : ""), `${mark} ${c.name}  ${c.phase}`));
          if (c.output) call.append(el("pre", "", c.output.length > 2000 ? c.output.slice(0, 2000) + "\n…" : c.output));
          c.files.forEach((f) => {
            const wrap = el("div");
            wrap.innerHTML = f.diff_html;
            call.append(wrap);
          });
          call.addEventListener("click", () => send({ type: "open_tool", view, group: i, item: j }));
          body.append(call);
        });
      }
      d.append(body);
      sum.addEventListener("click", (e) => {
        e.preventDefault();
        d.open = !d.open;
        folds.set(foldKey, d.open);
      });
      node.append(d);
    } else {
      const body = el("div", row.kind === "user" ? "bubble" : row.kind === "agent" ? "body" : "");
      body.innerHTML = row.html;
      node.append(body);
      if (row.kind === "agent" && row.elapsed_ms) {
        node.append(el("div", "work", `工作 ${(row.elapsed_ms / 1000).toFixed(1)}s`));
      }
    }
    const pics = row.path ? [row.path] : (row.images || []);
    if (pics.length) {
      const box = el("div", "pics");
      pics.forEach((p) => {
        const img = el("img");
        img.src = mediaUrl(p);
        img.alt = row.label || p;
        img.addEventListener("click", () => send({ type: "open_image", path: p }));
        box.append(img);
      });
      (node.querySelector(".bubble") || node).append(box);
    }
    return node;
  }

  let chatKey = "";
  function renderChat() {
    const key = `${snap.session_id}|${activeTab}`;
    const changed = key !== chatKey;
    if (changed) {
      chatKey = key;
      chat.replaceChildren();
      const saved = scrollPositions.get(activeTab);
      stick = saved ? saved.stick : true;
    }
    const rows = viewRows(activeTab);
    while (chat.children.length > rows.length) chat.lastElementChild.remove();
    rows.forEach((row, i) => {
      const sig = rowSig(row);
      const existing = chat.children[i];
      if (existing && existing.dataset.sig === sig) {
        patchThinkClock(existing, row);
        return;
      }
      const next = buildChatRow(row, i, activeTab);
      if (existing) existing.replaceWith(next);
      else chat.append(next);
    });
    if (!rows.length && activeTab) {
      chat.append(el("div", "row meta", "尚無工作紀錄"));
    }
    if (stick) chat.scrollTop = chat.scrollHeight;
    else if (changed && scrollPositions.has(activeTab)) chat.scrollTop = scrollPositions.get(activeTab).top;
    $("jump-bottom").classList.toggle("hidden", stick);
  }

  chat.addEventListener("scroll", () => {
    stick = chat.scrollHeight - chat.scrollTop - chat.clientHeight < 80;
    $("jump-bottom").classList.toggle("hidden", stick);
  });

  // —— Composer / read-only footer ——
  let queueKey = "";
  let pendingKey = "";
  function renderEditorFooter() {
    const viewing = !!activeTab;
    $("composer-wrap").classList.toggle("hidden", viewing);
    $("tray").classList.toggle("hidden", viewing);
    const ro = $("readonly");
    ro.classList.toggle("hidden", !viewing);
    if (viewing) {
      const a = agentOf(activeTab);
      ro.replaceChildren();
      if (a) {
        const strong = el("strong", "state-" + a.state, `${STATE_ICON[a.state] || ""} ${a.path}`);
        const bits = [a.label, a.model, a.turn ? `第${a.turn}輪` : "", `工具 ${a.tools}`].filter(Boolean).join(" · ");
        const info = el("button", "chip", "任務說明");
        info.type = "button";
        info.addEventListener("click", () => send({ type: "open_agent_info", path: a.path }));
        ro.append(strong, document.createTextNode(`  ${bits}  ·  唯讀：子代理由主代理指揮  `), info);
      }
    }
    const qk = JSON.stringify(snap.queue || []);
    if (qk !== queueKey) {
      queueKey = qk;
      const box = $("queue");
      box.replaceChildren();
      (snap.queue || []).forEach((q, i) => {
        const p = el("button", "pill", q.text || `${q.images} 張圖`);
        p.type = "button";
        p.title = "編輯待處理訊息";
        p.addEventListener("click", () => send({ type: "edit_queue", index: i }));
        box.append(p);
      });
    }
    const pk = JSON.stringify(snap.pending || []);
    if (pk !== pendingKey) {
      pendingKey = pk;
      const box = $("pending");
      box.replaceChildren();
      (snap.pending || []).forEach((p, i) => {
        const b = el("button", "pill", p + " ✕");
        b.type = "button";
        b.addEventListener("click", () => send({ type: "remove_pending", index: i }));
        box.append(b);
      });
    }
    const liveAgents = logs.agents.some((a) => a.state === "working" || a.state === "starting");
    $("interrupt").classList.toggle("hidden", !snap.header.running && !liveAgents);
    $("interrupt").textContent = snap.header.running ? "停止目前工作" : "中斷子代理";
    $("mode-queue").classList.toggle("on", sendMode !== "insert");
    $("mode-insert").classList.toggle("on", sendMode === "insert");
    composer.placeholder = isCustom()
      ? "傳給自訂模型… Enter 送出，Shift+Enter 換行，Ctrl+Enter 調整目前工作"
      : "傳給 Grok… Enter 送出，Shift+Enter 換行，Ctrl+Enter 調整目前工作";
    resizeComposer();
  }

  function resizeComposer() {
    composer.style.height = "auto";
    composer.style.height = Math.min(200, composer.scrollHeight) + "px";
  }

  function pushComposer() {
    drafts.set(draftSession, composer.value);
    if (!awaitingReceipt) $("delivery").textContent = connected ? "草稿只保留在此分頁" : "離線草稿 · 重連後可送出";
    resizeComposer();
  }

  composer.addEventListener("compositionstart", () => { composing = true; });
  composer.addEventListener("compositionend", () => { composing = false; pushComposer(); });
  composer.addEventListener("input", () => { if (!composing) pushComposer(); });
  composer.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey && !composing && !e.isComposing) {
      e.preventDefault();
      submitMessage(e.ctrlKey || e.metaKey);
    }
  });

  // —— Bottom panel ——
  function setPanel(tab) {
    panelTab = tab === undefined ? (panelTab ? null : "tools") : tab;
    renderAll();
  }

  let panelKey = "";
  function renderPanel() {
    const panel = $("panel");
    panel.classList.toggle("hidden", !panelTab);
    if (!panelTab) { panelKey = ""; return; }
    const key = JSON.stringify([panelTab, outputPick,
      panelTab === "tools" ? logs.tools : null,
      panelTab === "events" ? logs.events : null,
      panelTab === "output" ? logs.rail.backgrounds : null]);
    if (key === panelKey) return;
    panelKey = key;
    const tabs = $("panel-tabs");
    tabs.replaceChildren();
    const running = logs.tools.filter((t) => !t.done).length;
    const liveBg = logs.rail.backgrounds.filter((b) => b.alive).length;
    [["tools", "工具", running], ["output", "輸出", liveBg], ["events", "事件", 0]].forEach(([id, label, n]) => {
      const b = el("button", "ptab" + (panelTab === id ? " on" : ""), label);
      b.type = "button";
      b.setAttribute("role", "tab");
      if (n) b.append(el("span", "count", String(n)));
      b.addEventListener("click", () => setPanel(id));
      tabs.append(b);
    });
    const body = $("panel-body");
    const atBottom = body.scrollHeight - body.scrollTop - body.clientHeight < 30;
    body.replaceChildren();
    if (panelTab === "tools") {
      if (!logs.tools.length) body.append(el("div", "side-note", "尚無工具呼叫"));
      logs.tools.forEach((t) => {
        const cls = !t.done ? "run" : t.phase === "失敗" ? "fail" : "";
        const line = el("div", "pline click " + cls);
        const mark = !t.done ? "◐" : t.phase === "失敗" ? "!" : t.phase === "已停止" ? "⊘" : "✓";
        line.append(el("span", "", mark), el("span", "who", t.path || "主"),
          el("span", "txt", `${t.line.replace(/^▸ /, "")}  ${t.phase}`),
          el("span", "ms", t.done ? `${(t.ms / 1000).toFixed(1)}s` : ""));
        line.title = "查看這次工具呼叫";
        line.addEventListener("click", () => revealTool(t.path, t.call_id));
        body.append(line);
      });
    } else if (panelTab === "events") {
      if (!logs.events.length) body.append(el("div", "side-note", "尚無事件"));
      logs.events.forEach((e) => {
        const line = el("div", "pline " + e.kind + (e.path ? " click" : ""));
        line.append(el("span", "at", e.at), el("span", "who", e.path || "主"), el("span", "txt", e.text));
        line.title = e.text;
        if (e.path) line.addEventListener("click", () => openAgent(e.path));
        body.append(line);
      });
    } else {
      const bgs = logs.rail.backgrounds;
      if (!bgs.length) { body.append(el("div", "side-note", "尚無背景行程輸出")); }
      else {
        const pick = bgs.find((b) => b.name === outputPick) || bgs.slice().reverse().find((b) => b.alive) || bgs[bgs.length - 1];
        const picks = el("div", "out-picks");
        bgs.forEach((b) => {
          const c = el("button", "chip" + (b === pick ? " on" : ""), `${b.alive ? "●" : "○"} ${b.name}`);
          c.type = "button";
          c.addEventListener("click", () => { outputPick = b.name; renderAll(); });
          picks.append(c);
        });
        body.append(picks, el("div", "hint", `$ ${pick.command} · ${pick.status}${pick.detail ? " · " + pick.detail : ""}`));
        const pre = el("pre", "out-log");
        (pick.log || []).forEach((l) => {
          const span = el("span", l.startsWith("stderr") ? "stderr" : "", l + "\n");
          pre.append(span);
        });
        body.append(pre);
      }
    }
    if (atBottom) body.scrollTop = body.scrollHeight;
  }

  function revealTool(path, callId) {
    const rows = viewRows(path);
    for (let r = rows.length - 1; r >= 0; r--) {
      const calls = rows[r].calls || [];
      const c = calls.findIndex((x) => x.call_id === callId);
      if (c >= 0) { send({ type: "open_tool", view: path, group: r, item: c }); return; }
    }
    if (path) openAgent(path);
  }

  // —— Status bar ——
  function renderStatus() {
    const h = snap.header;
    $("statusbar").classList.toggle("running", !!h.running);
    const left = $("status-left");
    left.replaceChildren();
    const clock = h.running ? ` ${(h.elapsed_ms / 1000).toFixed(1)}s` : "";
    const activity = snap.ask ? "等你回覆" : (h.activity || h.status || "待命");
    const act = el("span", "sb", `${h.running ? "◐" : "●"} ${activity}${clock}`);
    act.title = h.status;
    left.append(act);
    const live = logs.agents.filter((a) => a.state === "working" || a.state === "starting").length;
    if (logs.agents.length) {
      const b = el("button", "sb", `◎ 代理 ${live}/${logs.agents.filter((a) => a.alive).length}`);
      b.type = "button";
      b.title = "開啟代理檢視";
      b.addEventListener("click", () => setSide("agents"));
      left.append(b);
    }
    const tools = el("button", "sb wide", `⚙ 工具 ${logs.tools.filter((t) => !t.done).length}`);
    tools.type = "button";
    tools.title = "開啟工具面板 (Ctrl+J)";
    tools.addEventListener("click", () => setPanel(panelTab === "tools" ? null : "tools"));
    left.append(tools);
    if (h.workspace) left.append(el("span", "sb wide", `📂 ${h.workspace}`));

    const right = $("status-right");
    right.replaceChildren();
    right.append(el("span", "sb wide", h.cache));
    right.append(el("span", "sb wide", h.logged_in ? "已登入" : "未登入"));
    const task = el("button", "sb", h.task_live ? "任務 ●" : "任務");
    task.type = "button";
    task.addEventListener("click", () => send({ type: "open_task" }));
    const model = el("button", "sb", `${isCustom() ? "自訂 API" : "Grok"} · ${h.model}${h.effort ? " · " + h.effort : ""}`);
    model.type = "button";
    model.title = "設定";
    model.addEventListener("click", () => send({ type: "open_settings" }));
    right.append(task, model);
  }

  // —— Overlays ——
  function rememberSettingsFocus() {
    const ae = document.activeElement;
    if (!ae || !overlay.contains(ae)) { settingsFocus = null; return; }
    settingsFocus = { key: ae.getAttribute("data-field") || "", start: ae.selectionStart, end: ae.selectionEnd };
  }

  function restoreSettingsFocus(root) {
    if (!settingsFocus || !settingsFocus.key) return;
    const target = root.querySelector(`[data-field="${settingsFocus.key}"]`);
    if (!target) return;
    target.focus();
    if (typeof settingsFocus.start === "number" && target.setSelectionRange) {
      try { target.setSelectionRange(settingsFocus.start, settingsFocus.end ?? settingsFocus.start); } catch (_) {}
    }
  }

  function overlaySnapshot() {
    if (!snap) return null;
    return { ...snap, settings: null, task: null, rename: null, inspector: null, image_view: null,
      tool_panel: null, ...localView,
      settings: localView.settings ? snap.settings_data : null,
      task: localView.task ? { ...snap.task_summary, draft: taskDraft,
        mode: snap.task_summary?.goal ? "status" : "form" } : null };
  }

  function drawerOverlay(o) {
    return !!(o.settings || o.task || o.inspector || o.tool_panel || o.skill_view || o.agent_info);
  }

  function toolCall(tp) {
    const g = viewRows(tp.view)[tp.group];
    return g && g.calls ? g.calls[tp.item] : null;
  }

  function renderOverlay(o = overlaySnapshot()) {
    if (!o || overlayComposing) return;
    const key = JSON.stringify([o.session_id, o.settings,
      o.rename && o.rename.id,
      o.task && { ...o.task, draft: undefined },
      o.ask && { ...o.ask, options: o.ask.options.map(x => ({ ...x, value: undefined })) },
      o.picker, o.inspector && [o.inspector, logs.rail.monitors], o.image_view,
      o.skill_view, o.queue_editor, o.tool_panel && [o.tool_panel, toolCall(o.tool_panel)],
      o.agent_info && agentOf(o.agent_info)]);
    if (key === overlayKey) return;
    overlayKey = key;
    const active = overlay.contains(document.activeElement) ? document.activeElement : null;
    const activeValue = active && "value" in active ? active.value : null;
    rememberSettingsFocus();
    const show = !!(o.settings || o.ask || o.task || o.picker || o.inspector || o.image_view || o.skill_view
      || o.rename || o.tool_panel || o.queue_editor || o.agent_info);
    overlay.classList.toggle("hidden", !show);
    overlay.classList.toggle("centered", show && !drawerOverlay(o));
    if (!show) { overlay.replaceChildren(); settingsFocus = null; return; }

    // Keep the settings form mounted so typing does not lose focus.
    const existing = overlay.querySelector(".modal.settings");
    if (o.settings && existing && !o.ask && !o.task && !o.picker && !o.inspector && !o.image_view
      && !o.skill_view && !o.rename && !o.tool_panel && !o.agent_info) {
      buildSettingsModal(existing, o.settings);
      restoreSettingsFocus(existing);
      return;
    }

    overlay.replaceChildren();
    const modal = el("div", "modal");
    if (o.queue_editor) {
      modal.append(h2("編輯待處理訊息"));
      const input = el("textarea");
      input.rows = 6;
      input.value = o.queue_editor.original;
      modal.append(input);
      const notice = el("p", "queue-notice hint");
      notice.setAttribute("role", "status");
      modal.append(notice);
      addBtns(modal, [
        ["取消", () => send({ type: "close_queue_editor" })],
        ["儲存", () => {
          if (!connected) { notice.textContent = "未儲存：連線中斷，文字保留在此處"; return; }
          if (awaitingReceipt) { send(awaitingReceipt); return; }
          awaitingReceipt = { type: "update_queue", request_id: crypto.randomUUID(), session_id: o.session_id,
            index: o.queue_editor.index, expected: o.queue_editor.original, text: input.value };
          notice.textContent = "儲存中，請等待確認；未回應時可再次點選儲存確認同一筆修改";
          send(awaitingReceipt);
        }],
      ]);
    } else if (o.image_view) {
      modal.className = "modal lightbox";
      const img = el("img");
      img.src = mediaUrl(o.image_view);
      img.alt = o.image_view;
      modal.append(img);
      addClose(modal, () => send({ type: "close_image" }));
    } else if (o.rename) {
      modal.append(h2("改名"));
      const inp = el("input");
      inp.type = "text";
      inp.value = o.rename.text;
      inp.addEventListener("keydown", (e) => { if (e.key === "Enter" && !e.isComposing) send({ type: "commit_rename", text: inp.value }); });
      modal.append(inp);
      addBtns(modal, [["取消", () => send({ type: "cancel_rename" })], ["確定", () => send({ type: "commit_rename", text: inp.value })]]);
    } else if (o.task) {
      if (o.task.mode === "form") {
        modal.append(h2("任務目標"));
        const ta = el("textarea");
        ta.rows = 6;
        ta.value = o.task.draft || "";
        ta.placeholder = "這則對話要達成什麼？";
        ta.addEventListener("input", () => send({ type: "set_task_draft", text: ta.value }));
        modal.append(ta);
        addBtns(modal, [["取消", () => send({ type: "close_task" })], ["確定", () => send({ type: "submit_task" })]]);
      } else {
        modal.append(h2("任務模式  ·  " + (o.task.phase || "")));
        modal.append(el("p", "", "目標  " + (o.task.goal || "")));
        modal.append(el("pre", "", (o.task.checklist || []).map((i) => (i.done ? "[x] " : "[ ] ") + i.text).join("\n") || "（尚無檢查表）"));
        if (o.task.note) modal.append(el("p", "hint", o.task.note));
        addBtns(modal, [["關閉", () => send({ type: "close_task" })], ["結束任務", () => send({ type: "end_task" })]]);
      }
    } else if (o.ask) {
      modal.append(h2(o.ask.prompt));
      o.ask.options.forEach((opt, i) => {
        const row = el("div", "opt" + (opt.chosen ? " on" : ""));
        row.append(el("span", "", opt.chosen ? (o.ask.allow_multiple ? "☑" : "●") : (o.ask.allow_multiple ? "☐" : "○")), el("span", "", opt.label));
        row.addEventListener("click", () => send({ type: "ask_toggle", index: i }));
        if (opt.input) {
          const inp = el("input");
          inp.type = "text";
          inp.value = opt.value;
          inp.addEventListener("click", (e) => e.stopPropagation());
          inp.addEventListener("input", () => send({ type: "ask_fill", index: i, text: inp.value }));
          row.append(inp);
        }
        modal.append(row);
      });
      addBtns(modal, [["取消", () => send({ type: "ask_cancel" })], ["確定", () => send({ type: "ask_confirm" })]]);
    } else if (o.picker) {
      modal.append(h2("選擇工作目錄"));
      const inp = el("input");
      inp.type = "text";
      inp.value = o.picker.path;
      inp.addEventListener("input", () => send({ type: "ws_set_path", text: inp.value }));
      modal.append(inp);
      if (o.picker.notice) modal.append(el("p", "hint", o.picker.notice));
      o.picker.entries.forEach((e, i) => {
        const row = el("div", "entry" + (i === o.picker.cursor ? " on" : ""), (e.is_parent ? ".." : e.name) + (e.is_dir && !e.is_parent ? "/" : ""));
        row.addEventListener("click", () => send({ type: "ws_select", index: i }));
        row.addEventListener("dblclick", () => send({ type: "ws_enter" }));
        modal.append(row);
      });
      addBtns(modal, [["取消", () => send({ type: "ws_cancel" })], ["建立資料夾", () => send({ type: "ws_create" })], ["確定", () => send({ type: "ws_confirm" })]]);
    } else if (o.skill_view) {
      modal.append(h2(o.skill_view.title + "  ·  " + o.skill_view.origin));
      modal.append(el("pre", "", o.skill_view.body));
      addClose(modal, () => send({ type: "close_skill" }));
    } else if (o.inspector) {
      const m = logs.rail.monitors.find((x) => x.name === o.inspector.name);
      modal.append(h2("監控  " + o.inspector.name));
      modal.append(el("pre", "", m ? `pid ${m.pid}\n${m.command}\n${m.status}\n${m.detail}` : "已不存在"));
      addClose(modal, () => send({ type: "close_inspector" }));
    } else if (o.agent_info) {
      const a = agentOf(o.agent_info);
      modal.append(h2("子代理  " + o.agent_info));
      if (a) {
        const card = el("div", "agent-card");
        card.append(el("div", "state-" + a.state, `${STATE_ICON[a.state] || ""} ${a.label}${a.turn ? " · 第" + a.turn + "輪" : ""} · 工具 ${a.tools}`));
        if (a.model) card.append(el("div", "dim", "模型  " + a.model));
        if (a.activity) card.append(el("div", "dim", "目前  " + a.activity));
        modal.append(card, el("h2", "", "任務說明"), el("pre", "", a.prompt || "（無）"));
      }
      addClose(modal, () => send({ type: "close_agent_info" }));
    } else if (o.tool_panel) {
      const c = toolCall(o.tool_panel);
      modal.append(h2((o.tool_panel.view ? `[${o.tool_panel.view}] ` : "") + (c ? `${c.name} · ${c.phase}` : "工具")));
      if (c) {
        modal.append(el("pre", "", c.args));
        if (c.output) modal.append(el("pre", "", c.output));
        c.files.forEach((f) => {
          const wrap = el("div");
          wrap.innerHTML = f.diff_html;
          modal.append(wrap);
        });
      } else {
        modal.append(el("p", "hint", "這次工具呼叫已不在紀錄中"));
      }
      addClose(modal, () => send({ type: "close_tool" }));
    } else if (o.settings) {
      modal.classList.add("settings");
      buildSettingsModal(modal, o.settings);
    }
    overlay.append(modal);
    modal.setAttribute("role", "dialog");
    modal.setAttribute("aria-modal", "true");
    modal.setAttribute("aria-label", modal.querySelector("h2")?.textContent || "詳情");
    modal.querySelectorAll("input, textarea, select").forEach((x, i) => { if (!x.dataset.field) x.dataset.field = "input-" + i; });
    if (settingsFocus && activeValue !== null) {
      const input = modal.querySelector(`[data-field="${settingsFocus.key}"]`);
      if (input) input.value = activeValue;
    }
    restoreSettingsFocus(modal);
    if (!settingsFocus) modal.querySelector("input, textarea, button")?.focus();
  }

  function buildSettingsModal(modal, st) {
    modal.replaceChildren();
    modal.append(h2("設定"));
    const kindRow = el("div", "field");
    kindRow.append(label("連線方式"));
    const seg = el("div", "seg");
    [["xai", "Grok"], ["openai", "自訂 API"]].forEach(([id, name]) => {
      const b = el("button", (st.kind || "xai") === id ? "on" : "", name);
      b.type = "button";
      b.addEventListener("click", () => send({ type: "set_provider_kind", kind: id }));
      seg.append(b);
    });
    kindRow.append(seg);
    modal.append(kindRow);

    if ((st.kind || "xai") === "openai") {
      modal.append(el("p", "hint", "填寫 OpenAI 相容端點。切回 Grok 會自動恢復 Grok 模型，自訂端點仍會保留。"));
      modal.append(textField("端點", "endpoint", st.base_url || "", (v) => send({ type: "set_endpoint", text: v })));
      if (st.custom_models) {
        modal.append(selectField("模型（端點＋Grok）", "model", st.models, snap.header.model, (id) => send({ type: "set_model", id })));
        modal.append(selectField("子代理模型", "child_model", [["", "跟隨主模型"], ...(st.models || [])], st.child_model || "", (id) => send({ type: "set_child_model", id })));
      } else {
        modal.append(textField("模型名", "model", snap.header.model, (v) => send({ type: "set_model", id: v })));
        modal.append(textField("子代理模型（留空＝同主模型）", "child_model", st.child_model || "", (v) => send({ type: "set_child_model", id: v })));
      }
      modal.append(textField("API 金鑰", "api_key", st.api_key || "", (v) => send({ type: "set_api_key", text: v }), true));
      modal.append(textField("上下文", "context", st.context || "", (v) => send({ type: "set_context", text: v })));
    } else {
      modal.append(el("p", "hint", "使用 xAI Grok 訂閱 OAuth。若剛從自訂 API 切換，模型會回到 Grok 目錄。"));
      const acc = el("div", "field");
      acc.append(label("帳號"));
      const btn = el("button");
      btn.type = "button";
      if (st.login === "waiting") {
        btn.textContent = "取消登入";
        btn.addEventListener("click", () => send({ type: "login" }));
        acc.append(btn, el("p", "hint", `在瀏覽器核准：${st.login_code || ""}`));
        if (st.login_url) {
          const a = el("a", "", "開啟登入頁");
          a.href = st.login_url;
          a.target = "_blank";
          a.rel = "noreferrer";
          acc.append(a);
        }
      } else if (snap.header.logged_in) {
        btn.textContent = "登出";
        btn.addEventListener("click", () => send({ type: "logout" }));
        acc.append(btn);
      } else {
        btn.textContent = st.login === "starting" ? "連線中…" : "登入 Grok";
        btn.addEventListener("click", () => send({ type: "login" }));
        acc.append(btn);
      }
      modal.append(acc);
      modal.append(selectField("模型", "model", st.models, snap.header.model, (id) => send({ type: "set_model", id })));
      modal.append(selectField("子代理模型", "child_model", [["", "跟隨主模型"], ...(st.models || [])], st.child_model || "", (id) => send({ type: "set_child_model", id })));
      modal.append(selectField("思考強度", "effort", st.efforts, effortId(st), (id) => send({ type: "set_effort", id })));
      const search = el("button", "", st.web_search ? "搜尋：開" : "搜尋：關");
      search.type = "button";
      search.addEventListener("click", () => send({ type: "toggle_search" }));
      modal.append(search);
    }
    const disp = el("button", "", st.dispatcher ? "調度員模式：開" : "調度員模式：關");
    disp.type = "button";
    disp.title = "開啟後模型只負責規劃並指揮子代理，盡量不直接動手";
    disp.addEventListener("click", () => send({ type: "toggle_dispatcher" }));
    modal.append(disp);
    const ic = el("button", "", st.import_claude ? "Claude 技能：開" : "Claude 技能：關");
    ic.type = "button";
    ic.addEventListener("click", () => send({ type: "toggle_import_claude" }));
    const ix = el("button", "", st.import_codex ? "Codex 技能：開" : "Codex 技能：關");
    ix.type = "button";
    ix.addEventListener("click", () => send({ type: "toggle_import_codex" }));
    modal.append(ic, ix);
    st.skills.forEach((sk, i) => {
      const row = el("div", "skill");
      const tog = el("button", "", sk.enabled ? "開" : "關");
      tog.type = "button";
      tog.addEventListener("click", () => send({ type: "toggle_skill", index: i }));
      const name = el("span", "", `${sk.name}  (${sk.origin})`);
      name.style.cursor = "pointer";
      name.addEventListener("click", () => send({ type: "open_skill", index: i }));
      row.append(tog, name);
      modal.append(row);
    });
    addClose(modal, () => send({ type: "close_settings" }));
  }

  function effortId(st) {
    const hit = (st.efforts || []).find((e) => e[1] === snap.header.effort || e[0] === snap.header.effort);
    return hit ? hit[0] : "";
  }
  function h2(t) { return el("h2", "", t); }
  function label(t) { return el("label", "", t); }
  function addClose(modal, fn) { addBtns(modal, [["關閉", fn]]); }
  function addBtns(modal, items) {
    const row = el("div", "row-btns");
    items.forEach(([t, fn]) => {
      const b = el("button", "", t);
      b.type = "button";
      b.addEventListener("click", fn);
      row.append(b);
    });
    modal.append(row);
  }
  function textField(title, key, current, onChange, secret) {
    const wrap = el("div", "field");
    wrap.append(label(title));
    const inp = el("input");
    inp.type = secret ? "password" : "text";
    inp.value = current || "";
    inp.setAttribute("data-field", key);
    inp.addEventListener("input", () => onChange(inp.value));
    inp.addEventListener("change", () => onChange(inp.value));
    wrap.append(inp);
    return wrap;
  }
  function selectField(title, key, pairs, current, onChange) {
    const wrap = el("div", "field");
    wrap.append(label(title));
    const sel = el("select");
    sel.setAttribute("data-field", key);
    (pairs || []).forEach(([id, name]) => {
      const opt = el("option", "", name || id);
      opt.value = id;
      if (id === current) opt.selected = true;
      sel.append(opt);
    });
    if (!(pairs || []).length) {
      const opt = el("option", "", current ? `${current}（載入目錄中…）` : "尚無模型");
      opt.value = current || "";
      opt.selected = true;
      sel.append(opt);
    }
    sel.addEventListener("change", () => onChange(sel.value));
    wrap.append(sel);
    return wrap;
  }

  // —— Wiring ——
  document.querySelectorAll(".act[data-view]").forEach((b) => b.addEventListener("click", () => setSide(b.dataset.view)));
  $("act-settings").addEventListener("click", () => send({ type: "open_settings" }));
  $("side-scrim").addEventListener("click", () => { sideOpen = false; renderAll(); });
  $("panel-close").addEventListener("click", () => setPanel(null));
  $("mode-queue").addEventListener("click", () => { sendMode = "queue"; renderAll(); });
  $("mode-insert").addEventListener("click", () => { sendMode = "insert"; renderAll(); });
  $("send-message").addEventListener("click", () => submitMessage());
  $("jump-bottom").addEventListener("click", () => { stick = true; chat.scrollTop = chat.scrollHeight; $("jump-bottom").classList.add("hidden"); });
  $("paste-image").addEventListener("click", () => send({ type: "paste_image" }));
  $("interrupt").addEventListener("click", () => send({ type: "interrupt" }));
  overlay.addEventListener("compositionstart", () => { overlayComposing = true; });
  overlay.addEventListener("compositionend", () => { overlayComposing = false; });
  overlay.addEventListener("click", (e) => {
    if (e.target !== overlay) return;
    const o = overlaySnapshot();
    if (!o) return;
    if (o.queue_editor) send({ type: "close_queue_editor" });
    else if (o.image_view) send({ type: "close_image" });
    else if (o.task) send({ type: "close_task" });
    else if (o.settings) send({ type: "close_settings" });
    else if (o.inspector) send({ type: "close_inspector" });
    else if (o.agent_info) send({ type: "close_agent_info" });
    else if (o.skill_view) send({ type: "close_skill" });
    else if (o.tool_panel) send({ type: "close_tool" });
    else if (o.ask) send({ type: "ask_cancel" });
    else if (o.picker) send({ type: "ws_cancel" });
    else if (o.rename) send({ type: "cancel_rename" });
  });
  document.addEventListener("keydown", (e) => {
    const mod = e.ctrlKey || e.metaKey;
    if (mod && !e.shiftKey && (e.key === "b" || e.key === "B")) { e.preventDefault(); setSide(); return; }
    if (mod && !e.shiftKey && (e.key === "j" || e.key === "J")) { e.preventDefault(); setPanel(); return; }
    if (e.altKey && /^[0-5]$/.test(e.key)) {
      e.preventDefault();
      if (e.key === "0") activate("");
      else setSide(["sessions", "agents", "changes", "background", "task"][Number(e.key) - 1]);
      return;
    }
    if (e.altKey && (e.key === "ArrowLeft" || e.key === "ArrowRight")) {
      e.preventDefault();
      cycleTab(e.key === "ArrowLeft" ? -1 : 1);
      return;
    }
    if (e.key === "Escape") {
      if (!overlay.classList.contains("hidden")) { overlay.click(); e.preventDefault(); }
      else if (sideOpen && narrow.matches) { sideOpen = false; renderAll(); }
      else if (activeTab) activate("");
      else if (snap?.header.running || logs.agents.some((a) => a.state === "working")) send({ type: "interrupt" });
      return;
    }
    if (e.key === "Tab" && !overlay.classList.contains("hidden")) {
      const items = [...overlay.querySelectorAll("button, input, textarea, select, [tabindex='0']")].filter(x => !x.disabled);
      const first = items[0], last = items[items.length - 1];
      if (items.length && (!overlay.contains(document.activeElement) || (e.shiftKey && document.activeElement === first) || (!e.shiftKey && document.activeElement === last))) {
        e.preventDefault(); (e.shiftKey ? last : first).focus();
      }
    }
  });
  narrow.addEventListener("change", () => { sideOpen = !narrow.matches; renderAll(); });

  function connect() {
    if (!token) {
      $("status-left").textContent = "缺少存取 token · 請從 TUI 開啟的網址進入";
      connectionState(false, "缺少連線資訊 · 請使用 TUI 提供的網址");
      return;
    }
    const proto = location.protocol === "https:" ? "wss" : "ws";
    ws = new WebSocket(`${proto}://${location.host}/ws?t=${encodeURIComponent(token)}`);
    ws.onmessage = (ev) => {
      let msg;
      try { msg = JSON.parse(ev.data); } catch { return; }
      if (msg.type === "hello") {
        connectionState(true, "● 已連線");
        onHello(msg);
      } else if (msg.type === "delta") {
        onDelta(msg);
      }
    };
    ws.onclose = () => {
      connectionState(false, "連線中斷 · 正在重新連線，草稿已保留");
      if (awaitingReceipt) $("delivery").textContent = "送出結果尚未確認，重連後將查詢接收紀錄";
      setTimeout(connect, 1500);
    };
  }
  connect();
})();
