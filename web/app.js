(() => {
  // The TUI's link carries a one-time code, traded below for the hub's key.
  // Neither it nor an old ?t= token may stay in the address bar.
  const linkCode = new URLSearchParams(location.search).get("code") || "";
  if (location.search) history.replaceState(null, "", location.pathname + location.hash);
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
  let taskDraft = { goal: "", criteria: "" };
  let taskFormNote = "";
  let taskEndArm = null;    // {at, sid}: 結束任務 pressed once in that session; a second press soon after ends it
  let apiKeyDraft = null;   // typed in this tab; the server never sends the key
  let overlayTop = null;
  let overlayShownAt = 0;
  const uploads = new Set();  // upload request ids waiting for the TUI's receipt
  let uploadChain = Promise.resolve();
  let uploadNote = "";      // what the last drop or paste could not attach
  const THUMB = 480;        // longest edge of chat and tray thumbnails

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

  // Authenticated by the media token from /auth; `max` asks for a scaled copy.
  function mediaUrl(path, max) {
    return `/media?path=${encodeURIComponent(path)}${max ? `&max=${max}` : ""}&t=${encodeURIComponent(mediaToken)}`;
  }

  function baseName(path) {
    return String(path).split(/[\\/]/).pop();
  }

  function wsSend(obj) {
    if (ws && ws.readyState === 1) { ws.send(JSON.stringify(obj)); return true; }
    return false;
  }

  function send(obj) {
    if (obj.type === "open_settings") {
      apiKeyDraft = null;
      localView = { settings: true }; renderOverlay();
      return send({ type: "refresh_settings" });
    }
    if (obj.type === "open_task") {
      taskDraft = { goal: "", criteria: "" };
      taskFormNote = "";
      localView = { task: { mode: snap.task_summary?.goal ? "status" : "form" } };
      renderOverlay(); return true;
    }
    // 修改: the same form, prefilled from the task.
    if (obj.type === "edit_task_form") {
      const t = snap.task_summary || {};
      taskDraft = { goal: t.goal || "", criteria: t.criteria || "" };
      taskFormNote = "";
      // The goal it was opened for: a task ended or replaced meanwhile
      // takes the form back to the status view.
      localView = { task: { mode: "edit", base: t.goal || "" } };
      renderOverlay(); return true;
    }
    if (obj.type === "task_status") { localView = { task: { mode: "status" } }; renderOverlay(); return true; }
    if (obj.type === "set_task_draft") { taskDraft.goal = obj.text; return true; }
    if (obj.type === "submit_task") {
      if (!taskDraft.goal.trim()) { taskFormNote = "請填寫任務目標"; renderOverlay(); return false; }
      taskFormNote = "";
      const editing = localView.task?.mode === "edit";
      const ok = send({ type: editing ? "edit_task" : "start_task", session_id: snap.session_id,
        goal: taskDraft.goal, criteria: taskDraft.criteria });
      if (ok) { localView = { task: { mode: "status" } }; renderOverlay(); }
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
      apiKeyDraft = null;
      localView = {}; renderOverlay(); return true;
    }
    // Reading details is local to this browser, independent of the terminal.
    const local = {
      open_tool: () => ({ tool_panel: { view: obj.view || "", group: obj.group, item: obj.item } }),
      open_image: () => ({ image_view: obj.path, image_source: obj.source || "" }),
      open_monitor: () => ({ inspector: { kind: "monitor", name: obj.name } }),
      open_agent_info: () => ({ agent_info: obj.path }),
      edit_queue: () => ({ queue_editor: { index: obj.index, original: snap.queue[obj.index].text,
        images: snap.queue[obj.index].paths || [] } }),
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
      taskEndArm = null;
      // Agent tabs belong to a session.
      openTabs.length = 0;
      activeTab = "";
    }
    snap = s;
    // A form this tab left open does not outlive the task it was for: a
    // task started elsewhere turns the new-task form into its status, and
    // an edit form whose task was ended or replaced goes back to status.
    const tv = localView.task;
    const goal = s.task_summary?.goal || "";
    if (tv && ((tv.mode === "form" && goal) || (tv.mode === "edit" && goal !== tv.base))) {
      localView = { task: { mode: "status" } };
    }
    if (awaitingReceipt) {
      const receipt = (s.receipts || []).find(([id]) => id === awaitingReceipt.request_id);
      if (receipt) {
        const request = awaitingReceipt;
        if (receipt[1] === "已接收") {
          if (request.type === "update_queue" || request.type === "remove_queued") {
            localView = {};
            $("delivery").textContent = request.type === "remove_queued" ? "已刪除待處理訊息" : "已儲存待處理訊息";
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
    settleUploads();
  }

  function settleUploads() {
    for (const [id, result] of (snap && snap.receipts) || []) {
      if (uploads.delete(id)) uploadStatus(result);
    }
  }

  function uploadStatus(text) {
    $("delivery").textContent = uploadNote ? `${text} · ${uploadNote}` : text;
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
      sideView === "task" ? [snap.task_summary, taskEndArmed()] : null]);
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
        body.append(el("div", "side-note", "尚未設定任務目標。任務模式讓用戶代理替你督促主代理，一直做到目標完成。"));
      } else {
        body.append(el("div", "side-section", "目標"), el("div", "side-note task-text", task.goal));
        if (task.criteria) body.append(el("div", "side-section", "驗收條件／偏好"), el("div", "side-note task-text", task.criteria));
        const done = (task.checklist || []).filter((i) => i.done).length;
        body.append(el("div", "side-section", `${taskState(task)} · ${done}/${(task.checklist || []).length}`));
        if (task.last_proxy) {
          body.append(el("div", "side-section proxy-head", "用戶代理最新訊息"), el("div", "side-note task-text proxy-note", task.last_proxy));
        }
        if ((task.checklist || []).length) body.append(el("div", "side-section", "進度筆記"));
        (task.checklist || []).forEach((i) => {
          body.append(el("div", "side-item check" + (i.done ? " done" : ""), `${i.done ? "✓" : "□"} ${i.text}`));
        });
        if (task.note) body.append(el("div", "side-note", task.note));
        const acts = el("div", "side-acts");
        (task.actions || []).forEach((name) => {
          const b = el("button", name === "end" ? "danger" : "", taskActionLabel(name));
          b.type = "button";
          b.title = `${TASK_ACTIONS[name][0]} (${TASK_ACTIONS[name][1]})`;
          b.addEventListener("click", () => taskAction(name));
          acts.append(b);
        });
        body.append(acts);
      }
      const btn = el("button", "side-btn", task.goal ? "開啟任務面板" : "設定任務目標");
      btn.type = "button";
      btn.title = "Alt+T";
      btn.addEventListener("click", () => send({ type: "open_task" }));
      body.append(btn);
    }
  }

  // —— Task mode ——
  // Label and the key that runs it in the status dialog.
  const TASK_ACTIONS = {
    pause: ["暫停", "P"], resume: ["繼續", "P"], review: ["立即審查", "R"],
    accept: ["判定完成", "D"], edit: ["修改", "E"], end: ["結束任務", "X"],
  };

  function taskState(t) {
    const hold = t.paused ? " · 已暫停" : t.stopped ? " · 已停止" : "";
    return `${t.phase || ""}${hold} · 回合 ${t.rounds || 0}`;
  }

  const TASK_END_CONFIRM_MS = 4000;
  function taskEndArmed() {
    return !!taskEndArm && !!snap && taskEndArm.sid === snap.session_id
      && performance.now() - taskEndArm.at < TASK_END_CONFIRM_MS;
  }

  function taskActionLabel(name) {
    return name === "end" && taskEndArmed() ? "再按一次確認結束" : TASK_ACTIONS[name][0];
  }

  // Commands carry the session this page shows, so a click that races a
  // session switch in the terminal does nothing.
  function taskAction(name) {
    const sid = snap.session_id;
    const armed = taskEndArmed();
    taskEndArm = null;
    if (name === "pause" || name === "resume") return send({ type: "set_task_paused", session_id: sid, paused: name === "pause" });
    if (name === "review") return send({ type: "review_task", session_id: sid });
    if (name === "accept") return send({ type: "accept_task", session_id: sid });
    if (name === "edit") return send({ type: "edit_task_form" });
    if (name === "end" && !armed) {
      // Ending wipes the goal, the progress notes and the proxy's memory:
      // one stray key or click must not do that.
      taskEndArm = { at: performance.now(), sid };
      $("delivery").textContent = "再按一次「結束任務」確認：目標、進度筆記和用戶代理的記憶都會清除";
      renderAll();
      setTimeout(() => { if (!taskEndArmed()) renderAll(); }, TASK_END_CONFIRM_MS + 50);
      return true;
    }
    if (name === "end") {
      const ok = send({ type: "end_task", session_id: sid });
      if (ok && localView.task) { localView = {}; renderOverlay(); }
      return ok;
    }
    return false;
  }

  // A letter in the status dialog: the action it names, if the task allows it.
  function taskKey(key) {
    const t = snap.task_summary || {};
    const acts = t.actions || [];
    const name = { p: acts.includes("resume") ? "resume" : "pause", r: "review", d: "accept", e: "edit", x: "end" }[key.toLowerCase()];
    if (!name) return false;
    if (acts.includes(name)) taskAction(name);
    else if (name === "review" && t.paused) $("delivery").textContent = "用戶代理已暫停，請先按繼續";
    return true;
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
      images: row.images, calls: row.calls, path: row.path, label: row.label, intent: row.intent,
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
      row.kind === "proxy" ? `用戶代理 · ${row.label || ""}` :
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
      const body = el("div", row.kind === "user" ? "bubble" : row.kind === "agent" ? "body" :
        row.kind === "proxy" ? `bubble proxy-bubble intent-${row.intent || ""}` : "");
      body.innerHTML = row.html;
      node.append(body);
      if (row.kind === "agent" && row.elapsed_ms) {
        node.append(el("div", "work", `工作 ${(row.elapsed_ms / 1000).toFixed(1)}s`));
      }
    }
    const pics = row.path ? [row.path] : (row.images || []);
    if (pics.length) {
      const box = el("div", "pics");
      pics.forEach((p) => box.append(chatPic(p, row.label, row.source)));
      (node.querySelector(".bubble") || node).append(box);
    }
    return node;
  }

  // An <img> that turns into a note when the file is gone or not an image.
  function picImg(path, onDone) {
    const img = el("img");
    img.alt = baseName(path);
    img.decoding = "async";
    img.addEventListener("load", () => onDone && onDone());
    img.addEventListener("error", () => {
      img.replaceWith(el("span", "pic-missing", `圖片無法顯示 · ${baseName(path)}`));
      if (onDone) onDone();
    });
    return img;
  }

  // A button, so the keyboard can open the viewer too. `source`: the file
  // the model was shown a copy of; the viewer opens it at full size.
  function chatPic(path, label, source) {
    const b = el("button", "pic");
    b.type = "button";
    b.title = label || path;
    b.setAttribute("aria-label", `開啟圖片 ${baseName(source || path)}`);
    // Its height is unknown until it loads; pin the view again if it was at the bottom.
    const img = picImg(path, () => {
      if (!b.querySelector("img")) b.disabled = true;
      keepStuck();
    });
    img.loading = "lazy";
    img.src = mediaUrl(path, THUMB);
    b.append(img);
    b.addEventListener("click", () => send({ type: "open_image", path, source }));
    return b;
  }

  function keepStuck() {
    if (stick) chat.scrollTop = chat.scrollHeight;
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
        const p = el("button", "pill", q.images ? `[${q.images} 張圖] ${q.text}`.trim() : q.text);
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
        const b = el("button", "pill pending-pic");
        b.type = "button";
        b.title = `${p} · 點一下移除`;
        b.setAttribute("aria-label", `移除圖片 ${baseName(p)}`);
        const img = el("img");
        img.alt = "";
        img.decoding = "async";
        img.addEventListener("error", () => img.remove());
        img.src = mediaUrl(p, THUMB);
        b.append(img, el("span", "", `${baseName(p)} ✕`));
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
    task.title = "任務面板 (Alt+T)";
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
      task: localView.task ? taskView(localView.task.mode) : null };
  }

  // The form keeps its drafts in this tab; only the status view follows the task.
  function taskView(mode) {
    const t = snap.task_summary || {};
    if (mode === "status") return t.goal ? { ...t, mode: "status", end_armed: taskEndArmed() } : { mode: "form", note: taskFormNote };
    return { mode, note: taskFormNote };
  }

  // Server-driven prompts come first: an agent's question must not hide
  // behind an image or a form this tab happens to have open.
  const OVERLAY_ORDER = ["ask", "picker", "queue_editor", "image_view", "rename", "task", "skill_view",
    "inspector", "agent_info", "tool_panel", "settings"];
  const DRAWERS = ["settings", "task", "inspector", "tool_panel", "skill_view", "agent_info"];

  function topOverlay(o) {
    return OVERLAY_ORDER.find((k) => o[k]) || null;
  }

  function toolCall(tp) {
    const g = viewRows(tp.view)[tp.group];
    return g && g.calls ? g.calls[tp.item] : null;
  }

  function renderOverlay(o = overlaySnapshot()) {
    if (!o || overlayComposing) return;
    const key = JSON.stringify([o.session_id, o.settings,
      o.rename && o.rename.id,
      o.task,
      o.ask && { ...o.ask, options: o.ask.options.map(x => ({ ...x, value: undefined })) },
      o.picker, o.inspector && [o.inspector, logs.rail.monitors], o.image_view, o.image_source,
      o.skill_view, o.queue_editor && { ...o.queue_editor, text: undefined },
      o.tool_panel && [o.tool_panel, toolCall(o.tool_panel)],
      o.agent_info && agentOf(o.agent_info)]);
    if (key === overlayKey) return;
    overlayKey = key;
    const active = overlay.contains(document.activeElement) ? document.activeElement : null;
    const activeValue = active && "value" in active ? active.value : null;
    rememberSettingsFocus();
    const top = topOverlay(o);
    // Typing and focus carry over only within the same dialog.
    const sameTop = top === overlayTop;
    if (!sameTop) { overlayTop = top; overlayShownAt = performance.now(); }
    overlay.classList.toggle("hidden", !top);
    overlay.classList.toggle("centered", !!top && !DRAWERS.includes(top));
    if (!top) { overlay.replaceChildren(); settingsFocus = null; return; }

    // Keep the settings form mounted so typing does not lose focus.
    const existing = overlay.querySelector(".modal.settings");
    if (top === "settings" && existing) {
      buildSettingsModal(existing, o.settings);
      restoreSettingsFocus(existing);
      return;
    }

    overlay.replaceChildren();
    const modal = el("div", "modal");
    if (top === "ask") {
      if (o.ask.from_proxy) modal.append(el("p", "ask-from", "任務代理想確認最終效果"));
      modal.append(h2(o.ask.prompt));
      // A stray Enter meant for the composer must not answer as the dialog appears.
      const settled = () => performance.now() - overlayShownAt > 400;
      o.ask.options.forEach((opt, i) => {
        const row = el("div", "opt" + (opt.chosen ? " on" : ""));
        row.tabIndex = 0;
        row.setAttribute("role", o.ask.allow_multiple ? "checkbox" : "radio");
        row.setAttribute("aria-checked", String(!!opt.chosen));
        row.dataset.field = "ask-opt-" + i;
        row.append(el("span", "", opt.chosen ? (o.ask.allow_multiple ? "☑" : "●") : (o.ask.allow_multiple ? "☐" : "○")), el("span", "", opt.label));
        row.addEventListener("click", () => send({ type: "ask_toggle", index: i }));
        row.addEventListener("keydown", (e) => {
          if (e.target !== row || e.isComposing) return;
          if (e.key === " ") { e.preventDefault(); send({ type: "ask_toggle", index: i }); }
          if (e.key === "Enter" && settled()) {
            e.preventDefault();
            // As in the terminal: Enter opens a field, answers a multiple
            // choice as ticked, and picks a single choice and answers.
            if (opt.input) { row.querySelector("input")?.focus(); send({ type: "ask_toggle", index: i }); return; }
            if (!o.ask.allow_multiple) send({ type: "ask_toggle", index: i });
            send({ type: "ask_confirm" });
          }
        });
        if (opt.input) {
          const inp = el("input");
          inp.type = "text";
          inp.value = opt.value;
          inp.dataset.field = "ask-fill-" + i;
          inp.addEventListener("click", (e) => e.stopPropagation());
          inp.addEventListener("input", () => send({ type: "ask_fill", index: i, text: inp.value }));
          inp.addEventListener("keydown", (e) => {
            if (e.key === "Enter" && !e.isComposing && settled()) {
              e.preventDefault();
              send({ type: "ask_fill", index: i, text: inp.value });
              send({ type: "ask_confirm" });
            }
          });
          row.append(inp);
        }
        modal.append(row);
      });
      modal.append(el("p", "hint", o.ask.allow_multiple
        ? "Tab 移動 · 空白鍵勾選 · 點選或填寫後按確定"
        : "Tab 移動 · Enter 選擇 · 可填寫項輸入後按 Enter"));
      addBtns(modal, [["取消", () => send({ type: "ask_cancel" })], ["確定", () => send({ type: "ask_confirm" })]]);
    } else if (top === "picker") {
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
    } else if (top === "queue_editor") {
      modal.append(h2("編輯待處理訊息"));
      const input = el("textarea");
      input.rows = 6;
      input.value = o.queue_editor.text ?? o.queue_editor.original;
      // Kept in the view, so a question that covers the editor does not lose it.
      input.addEventListener("input", () => { o.queue_editor.text = input.value; });
      modal.append(input);
      if (o.queue_editor.images.length) {
        const box = el("div", "pics");
        o.queue_editor.images.forEach((p) => {
          const img = picImg(p);
          img.src = mediaUrl(p, THUMB);
          box.append(img);
        });
        modal.append(box);
      }
      const notice = el("p", "queue-notice hint");
      notice.setAttribute("role", "status");
      modal.append(notice);
      // Saving and deleting share the receipt slot; a retry re-sends the same request.
      const request = (fields, waiting) => {
        if (!connected) { notice.textContent = "未送出：連線中斷，文字保留在此處"; return; }
        if (awaitingReceipt) { send(awaitingReceipt); return; }
        awaitingReceipt = { request_id: crypto.randomUUID(), session_id: o.session_id,
          index: o.queue_editor.index, expected: o.queue_editor.original, ...fields };
        notice.textContent = waiting;
        send(awaitingReceipt);
      };
      addBtns(modal, [
        ["刪除", () => request({ type: "remove_queued", images: o.queue_editor.images },
          "刪除中，請等待確認"), "danger"],
        ["取消", () => send({ type: "close_queue_editor" })],
        ["儲存", () => request({ type: "update_queue", text: input.value },
          "儲存中，請等待確認；未回應時可再次點選儲存確認同一筆修改")],
      ]);
    } else if (top === "image_view") {
      modal.className = "modal lightbox";
      // The original file at full size; the copy the model saw when the
      // original is gone.
      const seen = o.image_view;
      const full = o.image_source || seen;
      const name = baseName(full);
      const head = el("div", "lightbox-head");
      const original = el("a", "", "開啟原圖");
      original.href = mediaUrl(full);
      original.target = "_blank";
      original.rel = "noopener";
      head.append(h2(name), original);
      // Scrolls at actual size, so a tall page screenshot stays readable.
      const frame = el("div", "lightbox-frame");
      frame.tabIndex = 0;
      const img = el("img");
      img.alt = name;
      img.title = "點一下切換實際大小";
      img.addEventListener("click", () => modal.classList.toggle("actual"));
      let fellBack = full === seen;
      img.addEventListener("error", () => {
        if (!fellBack) {
          fellBack = true;
          original.textContent = "開啟模型看到的版本";
          original.href = mediaUrl(seen);
          frame.before(el("p", "hint", "原圖已不在，顯示模型看到的版本"));
          img.src = mediaUrl(seen);
          return;
        }
        frame.replaceChildren(el("p", "hint", "圖片無法顯示：檔案可能已刪除，或不是支援的圖片格式"));
      });
      img.src = mediaUrl(full);
      frame.append(img);
      modal.append(head, frame);
      addBtns(modal, [["實際大小", () => modal.classList.toggle("actual")], ["關閉", () => send({ type: "close_image" })]]);
    } else if (top === "rename") {
      modal.append(h2("改名"));
      const inp = el("input");
      inp.type = "text";
      inp.value = o.rename.text;
      inp.addEventListener("input", () => { o.rename.text = inp.value; });
      inp.addEventListener("keydown", (e) => { if (e.key === "Enter" && !e.isComposing) send({ type: "commit_rename", text: inp.value }); });
      modal.append(inp);
      addBtns(modal, [["取消", () => send({ type: "cancel_rename" })], ["確定", () => send({ type: "commit_rename", text: inp.value })]]);
    } else if (top === "task") {
      const t = o.task;
      if (t.mode === "form" || t.mode === "edit") {
        modal.append(h2(t.mode === "edit" ? "修改任務" : "任務目標"));
        modal.append(el("p", "hint", "Ctrl+Enter 確定 · Esc 取消"));
        const area = (title, key, rows, placeholder) => {
          const wrap = el("div", "field");
          wrap.append(label(title));
          const ta = el("textarea");
          ta.rows = rows;
          ta.value = taskDraft[key];
          ta.placeholder = placeholder;
          ta.dataset.field = "task-" + key;
          ta.addEventListener("input", () => { taskDraft[key] = ta.value; });
          ta.addEventListener("keydown", (e) => {
            if (e.key === "Enter" && (e.ctrlKey || e.metaKey) && !e.isComposing) { e.preventDefault(); send({ type: "submit_task" }); }
          });
          wrap.append(ta);
          return wrap;
        };
        modal.append(area("目標（必填）", "goal", 5, "這則對話要達成什麼？"));
        modal.append(area("驗收條件／偏好（選填）", "criteria", 4, "怎樣才算完成、你的偏好或限制"));
        if (t.note) modal.append(el("p", "hint warn", t.note));
        addBtns(modal, [["取消", () => send(t.mode === "edit" ? { type: "task_status" } : { type: "close_task" })],
          ["確定", () => send({ type: "submit_task" })]]);
      } else {
        modal.append(h2("任務模式  ·  " + taskState(t)));
        const section = (title, text, cls) => {
          modal.append(el("h3", cls || "", title));
          modal.append(el("div", "task-text " + (cls || ""), text));
        };
        section("目標", t.goal || "");
        if (t.criteria) section("驗收條件／偏好", t.criteria);
        if (t.last_proxy) section("用戶代理最新訊息", t.last_proxy, "proxy-msg");
        const done = (t.checklist || []).filter((i) => i.done).length;
        section(`進度筆記  ${done}/${(t.checklist || []).length}`,
          (t.checklist || []).map((i) => (i.done ? "✓ " : "□ ") + i.text).join("\n") || "（用戶代理尚未寫下進度）");
        if (t.note) modal.append(el("p", "hint", t.note));
        modal.append(el("p", "hint", "按鈕上的字母鍵可直接執行 · Esc 關閉"));
        const row = el("div", "row-btns task-acts");
        (t.actions || []).forEach((name) => {
          const b = el("button", name === "end" ? "danger" : "", `${taskActionLabel(name)} (${TASK_ACTIONS[name][1]})`);
          b.type = "button";
          // Pause and resume are one button: focus stays on it as it flips.
          b.dataset.field = "task-act-" + (name === "resume" ? "pause" : name);
          b.addEventListener("click", () => taskAction(name));
          row.append(b);
        });
        const close = el("button", "", "關閉");
        close.type = "button";
        close.dataset.field = "task-act-close";
        close.addEventListener("click", () => send({ type: "close_task" }));
        row.append(close);
        modal.append(row);
      }
    } else if (top === "skill_view") {
      modal.append(h2(o.skill_view.title + "  ·  " + o.skill_view.origin));
      modal.append(el("pre", "", o.skill_view.body));
      addClose(modal, () => send({ type: "close_skill" }));
    } else if (top === "inspector") {
      const m = logs.rail.monitors.find((x) => x.name === o.inspector.name);
      modal.append(h2("監控  " + o.inspector.name));
      modal.append(el("pre", "", m ? `pid ${m.pid}\n${m.command}\n${m.status}\n${m.detail}` : "已不存在"));
      addClose(modal, () => send({ type: "close_inspector" }));
    } else if (top === "agent_info") {
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
    } else if (top === "tool_panel") {
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
    } else if (top === "settings") {
      modal.classList.add("settings");
      buildSettingsModal(modal, o.settings);
    }
    overlay.append(modal);
    modal.setAttribute("role", "dialog");
    modal.setAttribute("aria-modal", "true");
    modal.setAttribute("aria-label", modal.querySelector("h2")?.textContent || "詳情");
    modal.querySelectorAll("input, textarea, select").forEach((x, i) => { if (!x.dataset.field) x.dataset.field = "input-" + i; });
    if (sameTop && settingsFocus && activeValue !== null) {
      const input = modal.querySelector(`[data-field="${settingsFocus.key}"]`);
      if (input) input.value = activeValue;
    }
    if (sameTop) restoreSettingsFocus(modal);
    if (sameTop && settingsFocus) return;
    if (top === "ask") {
      // Not a button: Enter meant for the composer must not answer or cancel it.
      modal.tabIndex = -1;
      (modal.querySelector("input") || modal).focus();
    } else if (top === "task" && o.task.mode === "status") {
      // As in the terminal, Enter closes until the user picks an action.
      modal.querySelector('[data-field="task-act-close"]')?.focus();
    } else {
      modal.querySelector("input, textarea, button")?.focus();
    }
  }

  // Backdrop click or Escape: dismiss what is shown, in renderOverlay's order.
  function dismissOverlay(escape) {
    const o = overlaySnapshot();
    const top = o && topOverlay(o);
    if (!top) return;
    // What just appeared is not hit by the second click of a double-click,
    // or a repeated Escape, meant for what was there before.
    if (performance.now() - overlayShownAt < 400) return;
    const close = {
      ask: escape ? { type: "ask_cancel" } : null,  // a stray click must not cancel a question
      picker: { type: "ws_cancel" },
      queue_editor: { type: "close_queue_editor" },
      image_view: { type: "close_image" },
      rename: { type: "cancel_rename" },
      task: o.task?.mode === "edit" ? { type: "task_status" } : { type: "close_task" },
      skill_view: { type: "close_skill" },
      inspector: { type: "close_inspector" },
      agent_info: { type: "close_agent_info" },
      tool_panel: { type: "close_tool" },
      settings: { type: "close_settings" },
    }[top];
    if (close) send(close);
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
        modal.append(taskModelField(st));
      } else {
        modal.append(textField("模型名", "model", snap.header.model, (v) => send({ type: "set_model", id: v })));
        modal.append(textField("子代理模型（留空＝同主模型）", "child_model", st.child_model || "", (v) => send({ type: "set_child_model", id: v })));
        modal.append(textField("任務代理模型（留空＝同主模型）", "task_model", st.task_model || "", (v) => send({ type: "set_task_model", id: v })));
      }
      modal.append(apiKeyField(st.has_api_key));
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
      modal.append(taskModelField(st));
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
    const ultra = el("button", "", st.ultracode ? "Ultracode：開" : "Ultracode：關");
    ultra.type = "button";
    ultra.title = "開啟後，每個實質任務都用工作流程（多代理編排）處理，追求最完整正確的結果";
    ultra.addEventListener("click", () => send({ type: "toggle_ultracode" }));
    modal.append(ultra);
    const size = el("button", "", "工作流程規模：" + (st.workflow_size || "中（10 個代理以下）"));
    size.type = "button";
    size.title = "告訴模型工作流程該用多少代理（建議值，不是硬性上限）；點一下切換";
    size.addEventListener("click", () => send({ type: "cycle_workflow_size" }));
    modal.append(size);
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

  // The model the task-mode user proxy runs on; empty follows the main model.
  function taskModelField(st) {
    return selectField("任務代理模型", "task_model", [["", "跟主模型相同"], ...(st.models || [])], st.task_model || "",
      (id) => send({ type: "set_task_model", id }));
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
    items.forEach(([t, fn, cls]) => {
      const b = el("button", cls || "", t);
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
  // The browser never gets the key. Typing only edits this tab's draft,
  // which survives the form being rebuilt by the next snapshot; Enter or
  // leaving the field replaces the key, and only 清除金鑰 clears it.
  function apiKeyField(isSet) {
    const wrap = textField("API 金鑰", "api_key", apiKeyDraft ?? "", (v) => { apiKeyDraft = v; }, true);
    const inp = wrap.querySelector("input");
    inp.placeholder = isSet ? "已設定 · 輸入新的金鑰後按 Enter 取代" : "未設定";
    inp.autocomplete = "off";
    const commit = () => {
      const v = inp.value.trim();
      if (v && send({ type: "set_api_key", text: v })) apiKeyDraft = null;
    };
    inp.addEventListener("change", commit);
    inp.addEventListener("keydown", (e) => {
      if (e.key === "Enter" && !e.isComposing) { e.preventDefault(); commit(); }
    });
    if (isSet && !apiKeyDraft) {
      const clear = el("button", "", "清除金鑰");
      clear.type = "button";
      clear.addEventListener("click", () => { apiKeyDraft = ""; send({ type: "set_api_key", text: "" }); });
      wrap.append(clear);
    }
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
  overlay.addEventListener("click", (e) => { if (e.target === overlay) dismissOverlay(false); });
  document.addEventListener("keydown", (e) => {
    const mod = e.ctrlKey || e.metaKey;
    if (mod && !e.shiftKey && (e.key === "b" || e.key === "B")) { e.preventDefault(); setSide(); return; }
    if (mod && !e.shiftKey && (e.key === "j" || e.key === "J")) { e.preventDefault(); setPanel(); return; }
    if (e.altKey && !mod && e.code === "KeyT") {
      e.preventDefault();
      // A toggle for the status view; an open form keeps what was typed.
      const shown = overlaySnapshot()?.task;
      if (!shown) send({ type: "open_task" });
      else if (shown.mode === "status") send({ type: "close_task" });
      return;
    }
    if (!mod && !e.altKey && e.key.length === 1 && overlayTop === "task" && localView.task?.mode === "status"
        && !editable(e.target) && taskKey(e.key)) {
      e.preventDefault();
      return;
    }
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
      if (!overlay.classList.contains("hidden")) { dismissOverlay(true); e.preventDefault(); }
      else if (sideOpen && narrow.matches) { sideOpen = false; renderAll(); }
      else if (activeTab) activate("");
      else if (snap?.header.running || logs.agents.some((a) => a.state === "working")) send({ type: "interrupt" });
      return;
    }
    if (e.key === "Tab" && !overlay.classList.contains("hidden")) {
      const items = [...overlay.querySelectorAll("button, input, textarea, select, a[href], [tabindex='0']")].filter(x => !x.disabled);
      const first = items[0], last = items[items.length - 1];
      if (items.length && (!overlay.contains(document.activeElement) || (e.shiftKey && document.activeElement === first) || (!e.shiftKey && document.activeElement === last))) {
        e.preventDefault(); (e.shiftKey ? last : first).focus();
      }
    }
  });
  narrow.addEventListener("change", () => { sideOpen = !narrow.matches; renderAll(); });

  // —— Images from this browser: drop anywhere, or paste ——
  const IMAGE_NAME = /\.(png|jpe?g|jfif|gif|webp|bmp)$/i;

  function imageFiles(files) {
    return [...(files || [])].filter((f) => f && (f.type.startsWith("image/") || IMAGE_NAME.test(f.name || "")));
  }

  function attachImages(files) {
    if (!snap || !connected) { $("delivery").textContent = "未附上：連線中斷"; return; }
    if (activeTab) { $("delivery").textContent = "子代理分頁是唯讀的：切回主對話再加入圖片"; return; }
    const images = imageFiles(files);
    if (!images.length) { $("delivery").textContent = "只能加入圖片（PNG、JPEG、GIF、WebP、BMP）"; return; }
    // Said here rather than by the server, which would have to refuse
    // mid-upload: the browser then reports only a dropped connection.
    const fits = images.filter((f) => !snap.upload_max || f.size <= snap.upload_max);
    const room = snap.pending_max ? Math.max(0, snap.pending_max - (snap.pending || []).length) : fits.length;
    const take = fits.slice(0, room);
    const notes = [];
    if (fits.length < images.length) notes.push(`${images.length - fits.length} 張超過 ${snap.upload_max >> 20} MiB`);
    if (take.length < fits.length) notes.push(`最多 ${snap.pending_max} 張圖片`);
    uploadNote = notes.length ? `未附上 ${images.length - take.length} 張：${notes.join("；")}` : "";
    if (!take.length) { $("delivery").textContent = uploadNote; return; }
    const sid = snap.session_id;
    // One at a time: they attach in order and the limit applies to each.
    take.forEach((f) => { uploadChain = uploadChain.then(() => uploadImage(f, sid)); });
  }

  async function uploadImage(file, sid) {
    const id = crypto.randomUUID();
    uploadStatus(`上傳圖片中… ${file.name || ""}`);
    try {
      const r = await fetch(`/upload?session_id=${encodeURIComponent(sid)}&request_id=${id}`,
        { method: "POST", body: file, headers: { [KEY_HEADER]: currentKey() } });
      if (r.ok) {
        // The TUI's receipt may already be here.
        uploads.add(id);
        settleUploads();
        return;
      }
      uploadStatus(r.status === 401 || r.status === 403 ? `未附上：存取已失效，請${LINK_HINT}重新開啟` : (await r.text()) || `未附上（${r.status}）`);
    } catch {
      // Refused mid-upload looks like a drop too; ask why.
      await checkAccess();
      uploadStatus(denied ? `未附上：存取已失效，請${LINK_HINT}重新開啟` : "未附上：連線中斷");
    }
  }

  const draggingFiles = (e) => [...((e.dataTransfer && e.dataTransfer.types) || [])].includes("Files");
  const editable = (t) => t instanceof Element && !!t.closest("textarea, input");
  // Unhandled, a file dropped anywhere replaces the workbench, and the
  // draft, with the file itself. Text dropped into a field still lands there.
  document.addEventListener("dragover", (e) => {
    if (!draggingFiles(e) && editable(e.target)) return;
    e.preventDefault();
    if (e.dataTransfer) e.dataTransfer.dropEffect = draggingFiles(e) ? "copy" : "none";
  });
  document.addEventListener("drop", (e) => {
    if (!draggingFiles(e) && editable(e.target)) return;
    e.preventDefault();
    if (draggingFiles(e) && overlay.classList.contains("hidden")) attachImages(e.dataTransfer.files);
  });
  // Text wins, as in the terminal; a screenshot on the clipboard has none.
  document.addEventListener("paste", (e) => {
    const data = e.clipboardData;
    if (!data || !overlay.classList.contains("hidden") || data.getData("text/plain")) return;
    const images = imageFiles([...data.items].filter((i) => i.kind === "file").map((i) => i.getAsFile()));
    if (!images.length) return;
    e.preventDefault();
    attachImages(images);
  });

  // —— Access ——
  // The hub's key lives in localStorage, which is per origin: a cookie
  // would also go to every other port on this host. The socket and uploads
  // send it; /auth trades it for the media token that /media URLs carry.
  const KEY_SLOT = "groka.key";
  const KEY_HEADER = "X-Groka-Key";
  const LINK_HINT = "用 TUI 狀態列或設定分頁顯示的網址（每個網址只能用一次）";
  let accessKey = "";
  let mediaToken = "";
  let linkUsed = false;     // this tab's link had been spent before it got here
  let denied = false;       // the hub said the key does not work

  function currentKey() {
    let k = accessKey;
    try { k = localStorage.getItem(KEY_SLOT) || k; } catch {}
    return /^[0-9a-f]{1,64}$/.test(k) ? k : "";
  }

  function keepKey(k) {
    accessKey = k;
    try { localStorage.setItem(KEY_SLOT, k); } catch {}
  }

  async function redeem() {
    if (!linkCode) return;
    try {
      const r = await fetch("/redeem", { method: "POST", body: linkCode, cache: "no-store" });
      if (r.ok) keepKey(await r.text());
      else linkUsed = r.status === 410;
    } catch {}
  }

  function deniedText() {
    return linkUsed
      ? "這個網址已經用過：請改用 TUI 狀態列或設定分頁顯示的最新網址，草稿已保留"
      : `需要重新開啟：${LINK_HINT}，草稿已保留`;
  }

  // A refused socket looks like any other drop; this tells them apart.
  async function checkAccess() {
    try {
      const r = await fetch("/auth", { cache: "no-store", headers: { [KEY_HEADER]: currentKey() } });
      if (r.ok) mediaToken = await r.text();
      denied = r.status === 401;
      if (denied && !connected) connectionState(false, deniedText());
    } catch {}
  }

  function connect() {
    const proto = location.protocol === "https:" ? "wss" : "ws";
    // A socket cannot carry headers: the key rides as a subprotocol.
    ws = new WebSocket(`${proto}://${location.host}/ws`, ["groka", `k.${currentKey()}`]);
    ws.onmessage = (ev) => {
      let msg;
      try { msg = JSON.parse(ev.data); } catch { return; }
      if (msg.type === "hello") {
        denied = linkUsed = false;
        connectionState(true, "● 已連線");
        // The key may have come from another tab after this one's /auth;
        // images that failed without the token are built again.
        if (msg.media && msg.media !== mediaToken) {
          mediaToken = msg.media;
          chatKey = pendingKey = overlayKey = "";
        }
        onHello(msg);
      } else if (msg.type === "delta") {
        onDelta(msg);
      }
    };
    ws.onclose = () => {
      connectionState(false, denied ? deniedText() : "連線中斷 · 正在重新連線，草稿已保留");
      if (awaitingReceipt) $("delivery").textContent = "送出結果尚未確認，重連後將查詢接收紀錄";
      // Opening the TUI's current link in any tab stores the key for this one too.
      checkAccess();
      setTimeout(connect, 1500);
    };
  }
  // The media token is in hand before the first rows ask for images.
  redeem().then(checkAccess).then(connect);

  // —— Theme: auto (follow the system) → dark → light ——
  const THEME_KEY = "groka.theme";
  const THEME_NAMES = { auto: "自動", dark: "深色", light: "淺色" };
  function applyTheme(pref) {
    const root = document.documentElement;
    const light = pref === "light" || (pref === "auto" && matchMedia("(prefers-color-scheme: light)").matches);
    root.dataset.themePref = pref;
    root.dataset.theme = light ? "light" : "dark";
    const btn = $("act-theme");
    if (btn) btn.title = `主題：${THEME_NAMES[pref]}（點擊切換）`;
  }
  function initTheme() {
    let pref = document.documentElement.dataset.themePref || "auto";
    if (!THEME_NAMES[pref]) pref = "auto";
    applyTheme(pref);
    $("act-theme").addEventListener("click", () => {
      const order = ["auto", "dark", "light"];
      pref = order[(order.indexOf(pref) + 1) % order.length];
      try { localStorage.setItem(THEME_KEY, pref); } catch (e) {}
      applyTheme(pref);
    });
    matchMedia("(prefers-color-scheme: light)").addEventListener("change", () => {
      if (pref === "auto") applyTheme("auto");
    });
  }
  initTheme();
})();
