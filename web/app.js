(() => {
  const params = new URLSearchParams(location.search);
  const token = params.get("t") || "";
  const $ = (id) => document.getElementById(id);
  const chat = $("chat");
  const composer = $("composer");
  const overlay = $("overlay");
  let snap = null;
  let composing = false;
  let stick = true;
  let ws;
  let settingsFocus = null;
  let railManual = false;
  let connected = false;
  let sendMode = "queue";
  let draftSession = "";
  let awaitingReceipt = null;
  let receiptRetryReady = false;
  const drafts = new Map();
  const folds = new Map();
  const scrollPositions = new Map();
  let overlayKey = "";
  let overlayComposing = false;
  let localView = {};
  let taskDraft = "";

  function connectionState(ok, message) {
    connected = ok;
    $("connection").textContent = message;
    $("connection").classList.toggle("offline", !ok);
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

  const appEl = $("app");
  const scrim = $("scrim");

  function setSidebar(open) {
    $("sidebar").inert = !open && !appEl.classList.contains("sidebar-pinned");
    appEl.classList.toggle("sidebar-open", open);
    if (open) {
      scrim.classList.remove("hidden");
      appEl.classList.remove("rail-open");
    } else if (!appEl.classList.contains("rail-open")) {
      scrim.classList.add("hidden");
    }
  }

  function setRailOpen(open) {
    $("rail").inert = !open && !appEl.classList.contains("rail-pinned");
    if (open) {
      $("rail").classList.remove("hidden");
      appEl.classList.add("rail-open");
      appEl.classList.remove("sidebar-open");
      scrim.classList.remove("hidden");
    } else {
      appEl.classList.remove("rail-open");
      if (!appEl.classList.contains("sidebar-open")) scrim.classList.add("hidden");
    }
  }

  function mediaUrl(path) {
    return `/media?t=${encodeURIComponent(token)}&path=${encodeURIComponent(path)}`;
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
    const views = {
      open_tool: () => ({ tool_panel: { group: obj.group, item: obj.item } }),
      open_image: () => ({ image_view: obj.path }),
      open_child: () => ({ inspector: { kind: "child", name: obj.name } }),
      open_monitor: () => ({ inspector: { kind: "monitor", name: obj.name } }),
      open_background: () => ({ inspector: { kind: "background", name: obj.name } }),
      edit_queue: () => ({ queue_editor: { index: obj.index, original: snap.queue[obj.index].text } }),
    };
    if (views[obj.type]) { localView = views[obj.type](); renderOverlay(); return true; }
    if (["close_tool", "close_image", "close_inspector", "close_queue_editor"].includes(obj.type)) {
      localView = {}; renderOverlay(); return true;
    }
    if (ws && ws.readyState === 1 && connected) { ws.send(JSON.stringify(obj)); return true; }
    $("delivery").textContent = "未送出：連線中斷，請重連後再操作";
    return false;
  }

  function providerKind() {
    const fromHeader = snap && snap.header && snap.header.kind;
    const fromSettings = snap && snap.settings && snap.settings.kind;
    return String(fromSettings || fromHeader || "xai").toLowerCase();
  }

  function isCustom() {
    return providerKind() === "openai";
  }

  function agentLabel() {
    if (isCustom()) return "助手";
    const model = (snap && snap.header && snap.header.model) || "";
    return model.toLowerCase().startsWith("grok") ? "grok" : "助手";
  }

  function applySnapshot(s) {
    if (draftSession !== s.session_id) {
      drafts.set(draftSession, composer.value);
      scrollPositions.set(draftSession, { top: chat.scrollTop, stick });
      draftSession = s.session_id;
      composer.value = drafts.get(draftSession) || "";
      localView = {};
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
    renderHeader();
    renderSessions();
    renderChat();
    renderQueue();
    renderPending();
    renderRail();
    renderComposer();
    renderOverlay();
    $("interrupt").classList.toggle("hidden", !s.header.running);
    $("mode-queue").classList.toggle("on", sendMode !== "insert");
    $("mode-insert").classList.toggle("on", sendMode === "insert");
    const hasRail = true;
    $("rail-toggle").classList.toggle("hidden", !hasRail);
    if (!hasRail) {
      railManual = false;
      setRailOpen(false);
      $("rail").classList.add("hidden");
    } else if (railManual) {
      setRailOpen(true);
    }
    composer.placeholder = isCustom()
      ? "傳給自訂模型… Enter 送出，Shift+Enter 換行"
      : "傳給 Grok… Enter 送出，Shift+Enter 換行";
  }

  function renderHeader() {
    const h = snap.header;
    const el = $("header");
    el.classList.toggle("running", h.running);
    const clock = h.running ? ` · ${(h.elapsed_ms / 1000).toFixed(1)}s` : "";
    const activity = h.activity || h.status || "待命";
    $("status-line").textContent = `${activity}${clock}`;
    $("meta-line").textContent = [
      h.logged_in ? "已登入" : "未登入",
      h.cache,
      h.workspace ? `📂 ${h.workspace}` : "",
    ].filter(Boolean).join("  ·  ");
    $("gear").textContent = `${h.model}${h.effort ? " · " + h.effort : ""}`;
    $("task").classList.toggle("on", !!h.task_live);
    $("task").textContent = h.task_live ? "任務●" : "任務";
    const badge = $("provider-badge");
    badge.classList.remove("hidden");
    badge.textContent = isCustom() ? "自訂 API" : "Grok";
  }

  let sessionsKey = "";
  let queueKey = "";
  let pendingKey = "";
  let railKey = "";
  let chatSession = "";

  function renderSessions() {
    const key = JSON.stringify(snap.sessions || []);
    if (key === sessionsKey) return;
    sessionsKey = key;
    const box = $("sessions");
    box.replaceChildren();
    for (const s of snap.sessions) {
      const div = document.createElement("div");
      div.className = "session" + (s.current ? " current" : "");
      const acts = document.createElement("div");
      acts.className = "acts";
      const ren = document.createElement("button");
      ren.type = "button";
      ren.textContent = "改名";
      ren.addEventListener("click", (e) => {
        e.stopPropagation();
        send({ type: "begin_rename", id: s.id });
      });
      const del = document.createElement("button");
      del.type = "button";
      del.textContent = "刪";
      del.addEventListener("click", (e) => {
        e.stopPropagation();
        send({ type: "delete_session", id: s.id });
      });
      acts.append(ren, del);
      const name = document.createElement("div");
      name.className = "name";
      name.textContent = s.name;
      const meta = document.createElement("div");
      meta.className = "meta";
      meta.textContent = `${s.status || "待命"} · ${s.folder}`;
      div.append(acts, name, meta);
      div.addEventListener("click", () => {
        send({ type: "switch", id: s.id });
        if (!appEl.classList.contains("sidebar-pinned")) setSidebar(false);
      });
      box.append(div);
    }
  }

  function rowSig(row) {
    // elapsed_ms ticks every pulse while running; keep it out of the identity
    // so completed rows are not rebuilt (which caused visible flicker).
    return JSON.stringify({
      kind: row.kind,
      html: row.html,
      text: row.text,
      expanded: row.expanded,
      done: row.done,
      images: row.images,
      calls: row.calls,
      path: row.path,
      label: row.label,
    }) + "\0" + agentLabel();
  }

  function patchThinkClock(el, row) {
    if (row.kind !== "think") return;
    const sum = el.querySelector("summary");
    if (!sum) return;
    const next = row.done
      ? `思考 ${((row.elapsed_ms || 0) / 1000).toFixed(1)}s`
      : "思考中";
    if (sum.textContent !== next) sum.textContent = next;
  }

  function buildChatRow(row, i) {
    const el = document.createElement("article");
    el.className = "row " + row.kind;
    el.dataset.sig = rowSig(row);
    const who = document.createElement("div");
    who.className = "who";
    who.textContent =
      row.kind === "user" ? "你" :
      row.kind === "agent" ? agentLabel() :
      row.kind === "think" ? "思考" :
      row.kind === "tools" ? "工具" :
      row.kind === "picture" ? "圖片" :
      row.kind === "err" ? "錯誤" : "系統";
    el.append(who);

    const wrapContent = (node) => {
      if (row.kind === "user") {
        const bubble = document.createElement("div");
        bubble.className = "bubble";
        bubble.append(node);
        el.append(bubble);
      } else if (row.kind === "agent") {
        node.classList.add("body");
        el.append(node);
      } else {
        el.append(node);
      }
    };

    if (row.kind === "think" || row.kind === "tools") {
      const d = document.createElement("details");
      d.className = "fold";
      const foldKey = `${snap.session_id}:${i}`;
      d.open = folds.get(foldKey) ?? !!row.expanded;
      const sum = document.createElement("summary");
      sum.textContent = row.kind === "think"
        ? (row.done ? `思考 ${((row.elapsed_ms || 0) / 1000).toFixed(1)}s` : "思考中")
        : `工具 ${row.calls.length}${row.calls.some(c => c.phase === "失敗") ? " · 有操作失敗" : row.calls.some(c => c.phase === "已停止") ? " · 已停止" : ""}`;
      d.append(sum);
      const body = document.createElement("div");
      body.innerHTML = row.html;
      if (row.kind === "tools") {
        row.calls.forEach((c, j) => {
          const call = document.createElement("div");
          call.className = "call";
          const t = document.createElement("div");
          const mark = c.phase === "失敗" ? "!" : c.phase === "已停止" ? "⊘" : c.done ? "✓" : "▸";
          t.textContent = `${mark} ${c.name}  ${c.phase}`;
          call.append(t);
          if (c.output) {
            const pre = document.createElement("pre");
            pre.textContent = c.output;
            call.append(pre);
          }
          c.files.forEach((f) => {
            const wrap = document.createElement("div");
            wrap.innerHTML = f.diff_html;
            call.append(wrap);
          });
          call.addEventListener("click", () => send({ type: "open_tool", group: i, item: j }));
          body.append(call);
        });
      }
      d.append(body);
      sum.addEventListener("click", (e) => {
        e.preventDefault();
        d.open = !d.open;
        folds.set(foldKey, d.open);
      });
      el.append(d);
    } else {
      const body = document.createElement("div");
      body.innerHTML = row.html;
      wrapContent(body);
    }
    if (row.images && row.images.length) {
      const pics = document.createElement("div");
      pics.className = "pics";
      row.images.forEach((p) => {
        const img = document.createElement("img");
        img.src = mediaUrl(p);
        img.alt = p;
        img.addEventListener("click", () => send({ type: "open_image", path: p }));
        pics.append(img);
      });
      if (row.kind === "user") {
        const bubble = el.querySelector(".bubble") || el;
        bubble.append(pics);
      } else {
        el.append(pics);
      }
    }
    if (row.path) {
      const pics = document.createElement("div");
      pics.className = "pics";
      const img = document.createElement("img");
      img.src = mediaUrl(row.path);
      img.alt = row.label || row.path;
      img.addEventListener("click", () => send({ type: "open_image", path: row.path }));
      pics.append(img);
      el.append(pics);
    }
    return el;
  }

  function renderChat() {
    const sid = snap.session_id || "";
    const changedSession = sid !== chatSession;
    if (changedSession) {
      chatSession = sid;
      chat.replaceChildren();
      stick = scrollPositions.get(sid)?.stick ?? true;
    }
    const rows = snap.rows || [];
    while (chat.children.length > rows.length) {
      chat.lastElementChild.remove();
    }
    rows.forEach((row, i) => {
      const sig = rowSig(row);
      const existing = chat.children[i];
      if (existing && existing.dataset.sig === sig) {
        patchThinkClock(existing, row);
        return;
      }
      const next = buildChatRow(row, i);
      if (existing) existing.replaceWith(next);
      else chat.append(next);
    });
    if (stick) chat.scrollTop = chat.scrollHeight;
    else if (changedSession && scrollPositions.has(sid)) chat.scrollTop = scrollPositions.get(sid).top;
    $("jump-bottom").classList.toggle("hidden", stick);
  }

  chat.addEventListener("scroll", () => {
    stick = chat.scrollHeight - chat.scrollTop - chat.clientHeight < 80;
    $("jump-bottom").classList.toggle("hidden", stick);
  });

  function renderQueue() {
    const key = JSON.stringify(snap.queue || []);
    if (key === queueKey) return;
    queueKey = key;
    const box = $("queue");
    box.replaceChildren();
    (snap.queue || []).forEach((q, i) => {
      const p = document.createElement("button");
      p.type = "button";
      p.className = "pill";
      p.textContent = q.text || `${q.images} 張圖`;
      p.addEventListener("click", () => send({ type: "edit_queue", index: i }));
      box.append(p);
    });
  }

  function renderPending() {
    const key = JSON.stringify(snap.pending || []);
    if (key === pendingKey) return;
    pendingKey = key;
    const box = $("pending");
    box.replaceChildren();
    (snap.pending || []).forEach((p, i) => {
      const b = document.createElement("button");
      b.type = "button";
      b.className = "pill";
      b.textContent = p + " ×";
      b.addEventListener("click", () => send({ type: "remove_pending", index: i }));
      box.append(b);
    });
  }

  function renderRail() {
    renderWorkSummary();
    const key = JSON.stringify(snap.rail || {});
    if (key === railKey) return;
    railKey = key;
    const box = $("rail-body");
    box.replaceChildren();
    const add = (kind, items, type) => {
      if (!items.length) return;
      const h = document.createElement("h3");
      h.textContent = kind;
      box.append(h);
      items.forEach((it) => {
        const d = document.createElement("div");
        d.className = "rail-item" + (it.alive ? " alive" : "");
        d.textContent = `${it.name}  ${it.status || ""}  ${it.activity || it.command || ""}`;
        d.addEventListener("click", () => send({ type, name: it.name }));
        box.append(d);
      });
    };
    add("子代理", snap.rail.children, "open_child");
    add("監控", snap.rail.monitors, "open_monitor");
    add("後台", snap.rail.backgrounds, "open_background");
  }

  function renderWorkSummary() {
    const box = $("work-summary");
    const task = snap.task_summary || {};
    const signature = JSON.stringify([task, snap.header.status, snap.header.activity, !!snap.ask]);
    if (box.dataset.signature !== signature) {
      box.dataset.signature = signature;
      box.replaceChildren();
      const title = document.createElement("h3");
      title.textContent = "目前工作";
      const status = document.createElement("p");
      status.className = "work-status";
      status.textContent = snap.ask ? "需要你回覆" : snap.header.status;
      const activity = document.createElement("p");
      activity.textContent = snap.header.activity || "等待下一個指示";
      const goal = document.createElement("p");
      goal.className = "work-goal";
      goal.textContent = task.goal || "尚未設定任務目標，可從上方「任務」建立。";
      box.append(title, status, activity, goal);
      if (task.goal) {
        const progress = document.createElement("p");
        progress.textContent = `${task.phase} · ${(task.checklist || []).filter(i => i.done).length}/${(task.checklist || []).length}`;
        box.append(progress);
      }
      for (const item of task.checklist || []) {
        const line = document.createElement("div");
        line.className = "check-item" + (item.done ? " done" : "");
        line.textContent = `${item.done ? "✓" : "□"} ${item.text}`;
        box.append(line);
      }
      if (task.note) {
        const note = document.createElement("p"); note.textContent = task.note; box.append(note);
      }
    }
    const changes = $("file-changes");
    const files = [];
    (snap.rows || []).forEach((row, group) => (row.calls || []).forEach((call, item) =>
      (call.files || []).forEach(file => files.push({ ...file, group, item }))));
    const key = JSON.stringify(files);
    if (changes.dataset.signature === key) return;
    changes.dataset.signature = key;
    changes.replaceChildren();
    const title = document.createElement("h3");
    title.textContent = `檔案變更 · ${new Set(files.map(f => f.path)).size}`;
    changes.append(title);
    if (!files.length) {
      const empty = document.createElement("p"); empty.className = "hint";
      empty.textContent = "這項工作尚無檔案變更紀錄"; changes.append(empty);
    }
    files.forEach(file => {
      const button = document.createElement("button");
      button.className = "file-change";
      button.textContent = file.path;
      button.title = "查看這次操作的差異";
      button.onclick = () => send({ type: "open_tool", group: file.group, item: file.item });
      changes.append(button);
    });
  }

  function renderComposer() {
    composer.style.height = "auto";
    composer.style.height = Math.min(180, composer.scrollHeight) + "px";
  }

  function pushComposer() {
    drafts.set(draftSession, composer.value);
    if (!awaitingReceipt) $("delivery").textContent = connected ? "草稿只保留在此分頁" : "離線草稿 · 重連後可送出";
    renderComposer();
  }

  composer.addEventListener("compositionstart", () => { composing = true; });
  composer.addEventListener("compositionend", () => {
    composing = false;
    pushComposer();
  });
  composer.addEventListener("input", () => {
    if (!composing) pushComposer();
  });
  composer.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey && !composing) {
      e.preventDefault();
      submitMessage(e.ctrlKey || e.metaKey);
    }
  });

  function rememberSettingsFocus() {
    const ae = document.activeElement;
    if (!ae || !overlay.contains(ae)) {
      settingsFocus = null;
      return;
    }
    settingsFocus = {
      key: ae.getAttribute("data-field") || "",
      start: ae.selectionStart,
      end: ae.selectionEnd,
    };
  }

  function restoreSettingsFocus(root) {
    if (!settingsFocus || !settingsFocus.key) return;
    const el = root.querySelector(`[data-field="${settingsFocus.key}"]`);
    if (!el) return;
    el.focus();
    if (typeof settingsFocus.start === "number" && el.setSelectionRange) {
      try {
        el.setSelectionRange(settingsFocus.start, settingsFocus.end ?? settingsFocus.start);
      } catch (_) {}
    }
  }

  function drawerOverlay(snap) {
    return !!(snap.settings || snap.task || snap.inspector || snap.tool_panel || snap.skill_view);
  }

  function overlaySnapshot() {
    return { ...snap, settings: null, task: null, rename: null,
      inspector: null, image_view: null, tool_panel: null, ...localView,
      settings: localView.settings ? snap.settings_data : null,
      task: localView.task ? { ...snap.task_summary, draft: taskDraft,
        mode: snap.task_summary?.goal ? "status" : "form" } : null };
  }

  function renderOverlay(snap = overlaySnapshot()) {
    if (!snap || overlayComposing) return;
    const key = JSON.stringify([snap.session_id, snap.settings,
      snap.rename && snap.rename.id,
      snap.task && { ...snap.task, draft: undefined },
      snap.ask && { ...snap.ask, options: snap.ask.options.map(o => ({ ...o, value: undefined })) },
      snap.picker, snap.inspector && [snap.inspector, snap.rail], snap.image_view,
      snap.skill_view, snap.queue_editor, snap.tool_panel && [snap.tool_panel, snap.rows[snap.tool_panel.group]]]);
    if (key === overlayKey) return;
    overlayKey = key;
    const active = overlay.contains(document.activeElement) ? document.activeElement : null;
    const activeValue = active && "value" in active ? active.value : null;
    rememberSettingsFocus();
    const show = !!(snap.settings || snap.ask || snap.task || snap.picker || snap.inspector || snap.image_view || snap.skill_view || snap.rename || snap.tool_panel || snap.queue_editor);
    overlay.classList.toggle("hidden", !show);
    overlay.classList.toggle("centered", show && !drawerOverlay(snap));
    if (!show) {
      overlay.replaceChildren();
      settingsFocus = null;
      return;
    }

    // Keep settings form mounted so typing endpoint/key does not steal focus.
    const existing = overlay.querySelector(".modal.settings");
    if (snap.settings && existing && !snap.ask && !snap.task && !snap.picker && !snap.inspector && !snap.image_view && !snap.skill_view && !snap.rename && !snap.tool_panel) {
      updateSettingsModal(existing, snap.settings);
      restoreSettingsFocus(existing);
      return;
    }

    overlay.replaceChildren();
    const modal = document.createElement("div");
    modal.className = "modal";
    if (snap.queue_editor) {
      modal.append(h2("編輯待處理訊息"));
      const input = document.createElement("textarea");
      input.value = snap.queue_editor.original;
      modal.append(input);
      const notice = document.createElement("p"); notice.className = "queue-notice"; notice.setAttribute("role", "status"); modal.append(notice);
      addBtns(modal, [
        ["取消", () => send({ type: "close_queue_editor" })],
        ["儲存", () => {
          if (!connected) { notice.textContent = "未儲存：連線中斷，文字保留在此處"; return; }
          if (awaitingReceipt) { send(awaitingReceipt); return; }
          awaitingReceipt = { type: "update_queue", request_id: crypto.randomUUID(), session_id: snap.session_id,
            index: snap.queue_editor.index, expected: snap.queue_editor.original, text: input.value };
          notice.textContent = "儲存中，請等待確認；未回應時可再次點選儲存確認同一筆修改";
          send(awaitingReceipt);
        }],
      ]);
    } else if (snap.image_view) {
      modal.className = "modal lightbox";
      const img = document.createElement("img");
      img.src = mediaUrl(snap.image_view);
      modal.append(img);
      addClose(modal, () => send({ type: "close_image" }));
    } else if (snap.rename) {
      modal.append(h2("改名"));
      const inp = document.createElement("input");
      inp.type = "text";
      inp.value = snap.rename.text;
      modal.append(inp);
      addBtns(modal, [
        ["取消", () => send({ type: "cancel_rename" })],
        ["確定", () => send({ type: "commit_rename", text: inp.value })],
      ]);
    } else if (snap.task) {
      if (snap.task.mode === "form") {
        modal.append(h2("任務目標"));
        const ta = document.createElement("textarea");
        ta.rows = 6;
        ta.value = snap.task.draft || "";
        ta.placeholder = "這則對話要達成什麼？";
        ta.addEventListener("input", () => send({ type: "set_task_draft", text: ta.value }));
        modal.append(ta);
        addBtns(modal, [
          ["取消", () => send({ type: "close_task" })],
          ["確定", () => send({ type: "submit_task" })],
        ]);
      } else {
        modal.append(h2("任務模式  ·  " + (snap.task.phase || "")));
        const goal = document.createElement("p");
        goal.textContent = "目標  " + (snap.task.goal || "");
        modal.append(goal);
        const list = document.createElement("pre");
        list.textContent = (snap.task.checklist || [])
          .map((i) => (i.done ? "[x] " : "[ ] ") + i.text)
          .join("\n") || "（尚無檢查表）";
        modal.append(list);
        addBtns(modal, [
          ["關閉", () => send({ type: "close_task" })],
          ["結束任務", () => send({ type: "end_task" })],
        ]);
      }
    } else if (snap.ask) {
      modal.append(h2(snap.ask.prompt));
      snap.ask.options.forEach((o, i) => {
        const row = document.createElement("div");
        row.className = "opt" + (o.chosen ? " on" : "");
        const mark = document.createElement("span");
        mark.textContent = o.chosen ? (snap.ask.allow_multiple ? "☑" : "●") : (snap.ask.allow_multiple ? "☐" : "○");
        const lab = document.createElement("span");
        lab.textContent = o.label;
        row.append(mark, lab);
        row.addEventListener("click", () => send({ type: "ask_toggle", index: i }));
        if (o.input) {
          const inp = document.createElement("input");
          inp.type = "text";
          inp.value = o.value;
          inp.addEventListener("click", (e) => e.stopPropagation());
          inp.addEventListener("input", () => send({ type: "ask_fill", index: i, text: inp.value }));
          row.append(inp);
        }
        modal.append(row);
      });
      addBtns(modal, [
        ["取消", () => send({ type: "ask_cancel" })],
        ["確定", () => send({ type: "ask_confirm" })],
      ]);
    } else if (snap.picker) {
      modal.append(h2("選擇工作目錄"));
      const inp = document.createElement("input");
      inp.type = "text";
      inp.value = snap.picker.path;
      inp.style.width = "100%";
      inp.addEventListener("input", () => send({ type: "ws_set_path", text: inp.value }));
      modal.append(inp);
      if (snap.picker.notice) {
        const n = document.createElement("p");
        n.textContent = snap.picker.notice;
        modal.append(n);
      }
      snap.picker.entries.forEach((e, i) => {
        const row = document.createElement("div");
        row.className = "entry" + (i === snap.picker.cursor ? " on" : "");
        row.textContent = (e.is_parent ? ".." : e.name) + (e.is_dir && !e.is_parent ? "/" : "");
        row.addEventListener("click", () => send({ type: "ws_select", index: i }));
        row.addEventListener("dblclick", () => send({ type: "ws_enter" }));
        modal.append(row);
      });
      addBtns(modal, [
        ["取消", () => send({ type: "ws_cancel" })],
        ["建立資料夾", () => send({ type: "ws_create" })],
        ["確定", () => send({ type: "ws_confirm" })],
      ]);
    } else if (snap.skill_view) {
      modal.append(h2(snap.skill_view.title + "  ·  " + snap.skill_view.origin));
      const pre = document.createElement("pre");
      pre.textContent = snap.skill_view.body;
      modal.append(pre);
      addClose(modal, () => send({ type: "close_skill" }));
    } else if (snap.inspector) {
      modal.append(h2(snap.inspector.kind + "  " + snap.inspector.name));
      const body = document.createElement("pre");
      if (snap.inspector.kind === "child") {
        const c = snap.rail.children.find((x) => x.name === snap.inspector.name);
        body.textContent = c ? `${c.status}\n${c.prompt}\n${c.card_url}\n\n${c.log.join("\n")}` : "";
      } else if (snap.inspector.kind === "monitor") {
        const m = snap.rail.monitors.find((x) => x.name === snap.inspector.name);
        body.textContent = m ? `pid ${m.pid}\n${m.command}\n${m.detail}` : "";
      } else {
        const b = snap.rail.backgrounds.find((x) => x.name === snap.inspector.name);
        body.textContent = b ? `pid ${b.pid}\n${b.command}\n${b.detail}\n\n${(b.log || []).join("\n")}` : "";
      }
      modal.append(body);
      addClose(modal, () => send({ type: "close_inspector" }));
    } else if (snap.tool_panel) {
      const g = snap.rows[snap.tool_panel.group];
      const c = g && g.calls ? g.calls[snap.tool_panel.item] : null;
      modal.append(h2(c ? c.name : "工具"));
      if (c) {
        const pre = document.createElement("pre");
        pre.textContent = c.args + "\n\n" + c.output;
        modal.append(pre);
        c.files.forEach((f) => {
          const wrap = document.createElement("div");
          wrap.innerHTML = f.diff_html;
          modal.append(wrap);
        });
      }
      addClose(modal, () => send({ type: "close_tool" }));
    } else if (snap.settings) {
      modal.classList.add("settings");
      buildSettingsModal(modal, snap.settings);
    }
    overlay.append(modal);
    modal.setAttribute("role", "dialog");
    modal.setAttribute("aria-modal", "true");
    modal.setAttribute("aria-label", modal.querySelector("h2")?.textContent || "詳情");
    modal.querySelectorAll("input, textarea, select").forEach((el, i) => {
      if (!el.dataset.field) el.dataset.field = "input-" + i;
    });
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
    const kindRow = document.createElement("div");
    kindRow.className = "field";
    kindRow.append(label("連線方式"));
    const seg = document.createElement("div");
    seg.className = "seg";
    [["xai", "Grok"], ["openai", "自訂 API"]].forEach(([id, name]) => {
      const b = document.createElement("button");
      b.type = "button";
      b.textContent = name;
      if ((st.kind || "xai") === id) b.classList.add("on");
      b.addEventListener("click", () => send({ type: "set_provider_kind", kind: id }));
      seg.append(b);
    });
    kindRow.append(seg);
    modal.append(kindRow);

    if ((st.kind || "xai") === "openai") {
      const hint = document.createElement("p");
      hint.className = "hint";
      hint.textContent = "填寫 OpenAI 相容端點。切回 Grok 會自動恢復 Grok 模型，自訂端點仍會保留。";
      modal.append(hint);
      modal.append(textField("端點", "endpoint", st.base_url || "", (v) => send({ type: "set_endpoint", text: v })));
      if (st.custom_models) {
        modal.append(selectField("模型（端點＋Grok）", "model", st.models, snap.header.model, (id) => send({ type: "set_model", id })));
        modal.append(selectField(
          "子代理模型",
          "child_model",
          [["", "跟隨主模型"], ...(st.models || [])],
          st.child_model || "",
          (id) => send({ type: "set_child_model", id }),
        ));
      } else {
        modal.append(textField("模型名", "model", snap.header.model, (v) => send({ type: "set_model", id: v })));
        modal.append(textField("子代理模型（留空＝同主模型）", "child_model", st.child_model || "", (v) => send({ type: "set_child_model", id: v })));
      }
      modal.append(textField("API 金鑰", "api_key", st.api_key || "", (v) => send({ type: "set_api_key", text: v }), true));
      modal.append(textField("上下文", "context", st.context || "", (v) => send({ type: "set_context", text: v })));
    } else {
      const hint = document.createElement("p");
      hint.className = "hint";
      hint.textContent = "使用 xAI Grok 訂閱 OAuth。若剛從自訂 API 切換，模型會回到 Grok 目錄。";
      modal.append(hint);
      const acc = document.createElement("div");
      acc.className = "field";
      acc.append(label("帳號"));
      const btn = document.createElement("button");
      btn.type = "button";
      if (st.login === "waiting") {
        btn.textContent = "取消登入";
        btn.addEventListener("click", () => send({ type: "login" }));
        const code = document.createElement("p");
        code.className = "hint";
        code.textContent = `在瀏覽器核准：${st.login_code || ""}`;
        acc.append(btn, code);
        if (st.login_url) {
          const a = document.createElement("a");
          a.href = st.login_url;
          a.target = "_blank";
          a.rel = "noreferrer";
          a.textContent = "開啟登入頁";
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
      modal.append(selectField(
        "子代理模型",
        "child_model",
        [["", "跟隨主模型"], ...(st.models || [])],
        st.child_model || "",
        (id) => send({ type: "set_child_model", id }),
      ));
      modal.append(selectField("思考強度", "effort", st.efforts, effortId(st), (id) => send({ type: "set_effort", id })));
      const search = document.createElement("button");
      search.type = "button";
      search.textContent = st.web_search ? "搜尋：開" : "搜尋：關";
      search.addEventListener("click", () => send({ type: "toggle_search" }));
      modal.append(search);
    }

    const disp = document.createElement("button");
    disp.type = "button";
    disp.textContent = st.dispatcher ? "調度員模式：開" : "調度員模式：關";
    disp.title = "開啟後模型只負責規劃並指揮子代理，盡量不直接動手";
    disp.addEventListener("click", () => send({ type: "toggle_dispatcher" }));
    modal.append(disp);

    const ic = document.createElement("button");
    ic.type = "button";
    ic.textContent = st.import_claude ? "Claude 技能：開" : "Claude 技能：關";
    ic.addEventListener("click", () => send({ type: "toggle_import_claude" }));
    const ix = document.createElement("button");
    ix.type = "button";
    ix.textContent = st.import_codex ? "Codex 技能：開" : "Codex 技能：關";
    ix.addEventListener("click", () => send({ type: "toggle_import_codex" }));
    modal.append(ic, ix);
    st.skills.forEach((sk, i) => {
      const row = document.createElement("div");
      row.className = "skill";
      const tog = document.createElement("button");
      tog.type = "button";
      tog.textContent = sk.enabled ? "開" : "關";
      tog.addEventListener("click", () => send({ type: "toggle_skill", index: i }));
      const name = document.createElement("span");
      name.textContent = `${sk.name}  (${sk.origin})`;
      name.style.cursor = "pointer";
      name.addEventListener("click", () => send({ type: "open_skill", index: i }));
      row.append(tog, name);
      modal.append(row);
    });
    addClose(modal, () => send({ type: "close_settings" }));
  }

  function updateSettingsModal(modal, st) {
    // Rebuild contents but keep the same modal node for focus restore.
    buildSettingsModal(modal, st);
  }

  function effortId(st) {
    const hit = (st.efforts || []).find((e) => e[1] === snap.header.effort || e[0] === snap.header.effort);
    return hit ? hit[0] : "";
  }

  function h2(t) {
    const e = document.createElement("h2");
    e.textContent = t;
    return e;
  }
  function label(t) {
    const e = document.createElement("label");
    e.textContent = t;
    return e;
  }
  function addClose(modal, fn) {
    addBtns(modal, [["關閉", fn]]);
  }
  function addBtns(modal, items) {
    const row = document.createElement("div");
    row.className = "row-btns";
    items.forEach(([t, fn]) => {
      const b = document.createElement("button");
      b.type = "button";
      b.textContent = t;
      b.addEventListener("click", fn);
      row.append(b);
    });
    modal.append(row);
  }
  function textField(title, key, current, onChange, secret) {
    const wrap = document.createElement("div");
    wrap.className = "field";
    wrap.append(label(title));
    const inp = document.createElement("input");
    inp.type = secret ? "password" : "text";
    inp.value = current || "";
    inp.setAttribute("data-field", key);
    inp.addEventListener("input", () => onChange(inp.value));
    inp.addEventListener("change", () => onChange(inp.value));
    wrap.append(inp);
    return wrap;
  }
  function selectField(title, key, pairs, current, onChange) {
    const wrap = document.createElement("div");
    wrap.className = "field";
    wrap.append(label(title));
    const sel = document.createElement("select");
    sel.setAttribute("data-field", key);
    (pairs || []).forEach(([id, name]) => {
      const o = document.createElement("option");
      o.value = id;
      o.textContent = name || id;
      if (id === current) o.selected = true;
      sel.append(o);
    });
    if (!(pairs || []).length) {
      const o = document.createElement("option");
      o.value = current || "";
      o.textContent = current ? `${current}（載入目錄中…）` : "尚無模型";
      o.selected = true;
      sel.append(o);
    }
    sel.addEventListener("change", () => onChange(sel.value));
    wrap.append(sel);
    return wrap;
  }

  $("new-chat").addEventListener("click", () => {
    setSidebar(false);
    send({ type: "new_chat" });
  });
  $("menu-toggle").addEventListener("click", () => {
    setSidebar(!appEl.classList.contains("sidebar-open"));
  });
  $("sidebar-close").addEventListener("click", () => setSidebar(false));
  $("rail-toggle").addEventListener("click", () => {
    railManual = !appEl.classList.contains("rail-open");
    setRailOpen(railManual);
  });
  $("rail-close").addEventListener("click", () => {
    railManual = false;
    setRailOpen(false);
  });
  scrim.addEventListener("click", () => {
    setSidebar(false);
    railManual = false;
    setRailOpen(false);
  });
  $("task").addEventListener("click", () => send({ type: "open_task" }));
  $("gear").addEventListener("click", () => send({ type: "open_settings" }));
  $("mode-queue").addEventListener("click", () => { sendMode = "queue"; if (snap) applySnapshot(snap); });
  $("mode-insert").addEventListener("click", () => { sendMode = "insert"; if (snap) applySnapshot(snap); });
  $("send-message").addEventListener("click", () => submitMessage());
  $("jump-bottom").addEventListener("click", () => { stick = true; chat.scrollTop = chat.scrollHeight; $("jump-bottom").classList.add("hidden"); });
  overlay.addEventListener("compositionstart", () => { overlayComposing = true; });
  overlay.addEventListener("compositionend", () => { overlayComposing = false; });
  $("paste-image").addEventListener("click", () => send({ type: "paste_image" }));
  $("interrupt").addEventListener("click", () => send({ type: "interrupt" }));
  overlay.addEventListener("click", (e) => {
    if (e.target === overlay) {
      const snap = overlaySnapshot();
      if (snap.queue_editor) { send({ type: "close_queue_editor" }); return; }
      if (snap && snap.image_view) send({ type: "close_image" });
      else if (snap && snap.task) send({ type: "close_task" });
      else if (snap && snap.settings) send({ type: "close_settings" });
      else if (snap && snap.inspector) send({ type: "close_inspector" });
      else if (snap && snap.skill_view) send({ type: "close_skill" });
      else if (snap && snap.tool_panel) send({ type: "close_tool" });
      else if (snap && snap.ask) send({ type: "ask_cancel" });
      else if (snap && snap.picker) send({ type: "ws_cancel" });
      else if (snap && snap.rename) send({ type: "cancel_rename" });
    }
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      if (!overlay.classList.contains("hidden")) { overlay.click(); e.preventDefault(); }
      else if (appEl.classList.contains("rail-open")) { railManual = false; setRailOpen(false); }
      else if (appEl.classList.contains("sidebar-open")) setSidebar(false);
      else if (snap?.header.running) send({ type: "interrupt" });
    }
    if (e.key === "Tab" && !overlay.classList.contains("hidden")) {
      const items = [...overlay.querySelectorAll("button, input, textarea, select, [tabindex='0']")].filter(el => !el.disabled);
      const first = items[0], last = items[items.length - 1];
      if (items.length && (!overlay.contains(document.activeElement) || (e.shiftKey && document.activeElement === first) || (!e.shiftKey && document.activeElement === last))) {
        e.preventDefault(); (e.shiftKey ? last : first).focus();
      }
    }
  });
  const wide = window.matchMedia("(min-width: 1200px)");
  function layout() {
    appEl.classList.toggle("sidebar-pinned", wide.matches);
    appEl.classList.toggle("rail-pinned", wide.matches);
    setSidebar(false); setRailOpen(false); railManual = false;
  }
  wide.addEventListener("change", layout);
  layout();

  function connect() {
    if (!token) {
      $("status-line").textContent = "缺少存取 token";
      $("meta-line").textContent = "請從 TUI 開啟的網址進入";
      connectionState(false, "缺少連線資訊 · 請使用 TUI 提供的網址");
      return;
    }
    const proto = location.protocol === "https:" ? "wss" : "ws";
    ws = new WebSocket(`${proto}://${location.host}/ws?t=${encodeURIComponent(token)}`);
    ws.onmessage = (ev) => {
      let msg;
      try { msg = JSON.parse(ev.data); } catch { return; }
      if (msg.type === "hello" || msg.type === "snapshot") {
        connectionState(true, "● 已連線 · 工作狀態即時同步");
        applySnapshot(msg.snapshot);
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
