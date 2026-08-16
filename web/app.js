(() => {
  const params = new URLSearchParams(location.search);
  const token = params.get("t") || "";
  const $ = (id) => document.getElementById(id);
  const chat = $("chat");
  const composer = $("composer");
  const overlay = $("overlay");
  let snap = null;
  let lastSentSeq = 0;
  let composing = false;
  let stick = true;
  let ws;

  function mediaUrl(path) {
    return `/media?t=${encodeURIComponent(token)}&path=${encodeURIComponent(path)}`;
  }

  function send(obj) {
    if (ws && ws.readyState === 1) ws.send(JSON.stringify(obj));
  }

  function applySnapshot(s) {
    snap = s;
    renderHeader();
    renderSessions();
    renderChat();
    renderQueue();
    renderPending();
    renderRail();
    renderComposer();
    renderOverlay();
    $("interrupt").classList.toggle("hidden", !s.header.running);
    $("mode-queue").classList.toggle("on", s.send_mode !== "insert");
    $("mode-insert").classList.toggle("on", s.send_mode === "insert");
    const hasRail = s.rail.children.length + s.rail.monitors.length + s.rail.backgrounds.length > 0;
    $("app").classList.toggle("has-rail", hasRail);
    $("rail").classList.toggle("hidden", !hasRail);
  }

  function renderHeader() {
    const h = snap.header;
    const el = $("header");
    el.classList.toggle("running", h.running);
    const clock = h.running ? `  ${(h.elapsed_ms / 1000).toFixed(1)}s` : "";
    $("header-left").textContent = `grokaagent  ${h.activity || h.status}${clock}  · ${h.status}  ${h.logged_in ? "已登入" : "未登入"}  ${h.cache}`;
    $("gear").textContent = `${h.model}${h.effort ? " " + h.effort : ""}  設定`;
  }

  function renderSessions() {
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
      meta.textContent = `${s.short_id}  ${s.folder}`;
      div.append(acts, name, meta);
      div.addEventListener("click", () => send({ type: "switch", id: s.id }));
      box.append(div);
    }
  }

  function renderChat() {
    const near = chat.scrollHeight - chat.scrollTop - chat.clientHeight < 80;
    chat.replaceChildren();
    snap.rows.forEach((row, i) => {
      const el = document.createElement("article");
      el.className = "row " + row.kind;
      const who = document.createElement("div");
      who.className = "who";
      who.textContent =
        row.kind === "user" ? "你" :
        row.kind === "agent" ? "grok" :
        row.kind === "think" ? "思考" :
        row.kind === "tools" ? "工具" :
        row.kind === "picture" ? "圖片" :
        row.kind === "err" ? "錯誤" : "系統";
      el.append(who);
      if (row.kind === "think" || row.kind === "tools") {
        const d = document.createElement("details");
        d.className = "fold";
        d.open = !!row.expanded;
        const sum = document.createElement("summary");
        sum.textContent = row.kind === "think"
          ? (row.done ? `思考 ${((row.elapsed_ms || 0) / 1000).toFixed(1)}s` : "思考中")
          : `工具 ${row.calls.length}`;
        d.append(sum);
        const body = document.createElement("div");
        body.innerHTML = row.html;
        if (row.kind === "tools") {
          row.calls.forEach((c, j) => {
            const call = document.createElement("div");
            call.className = "call";
            const t = document.createElement("div");
            t.textContent = `${c.done ? "✓" : "▸"} ${c.name}  ${c.phase}`;
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
          send({ type: "toggle_expand", index: i });
        });
        el.append(d);
      } else {
        const body = document.createElement("div");
        body.innerHTML = row.html;
        el.append(body);
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
        el.append(pics);
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
      chat.append(el);
    });
    if (stick || near) chat.scrollTop = chat.scrollHeight;
  }

  chat.addEventListener("scroll", () => {
    stick = chat.scrollHeight - chat.scrollTop - chat.clientHeight < 80;
  });

  function renderQueue() {
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

  function renderComposer() {
    const c = snap.composer;
    if (c.echo_seq === lastSentSeq) return;
    if (c.seq <= lastSentSeq) return;
    if (document.activeElement === composer && composing) return;
    if (composer.value !== c.text) composer.value = c.text;
    if (typeof c.caret === "number") composer.selectionStart = composer.selectionEnd = Math.min(c.caret, composer.value.length);
  }

  function pushComposer() {
    lastSentSeq += 1;
    send({
      type: "set_composer",
      text: composer.value,
      caret: composer.selectionStart || 0,
      seq: lastSentSeq,
    });
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
      send({ type: "submit", insert: e.ctrlKey || e.metaKey });
      lastSentSeq += 1;
    }
  });

  function renderOverlay() {
    overlay.replaceChildren();
    const show = !!(snap.settings || snap.ask || snap.picker || snap.inspector || snap.image_view || snap.skill_view || snap.rename || snap.tool_panel);
    overlay.classList.toggle("hidden", !show);
    if (!show) return;
    const modal = document.createElement("div");
    modal.className = "modal";
    if (snap.image_view) {
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
      const st = snap.settings;
      modal.append(h2("設定"));
      const acc = document.createElement("div");
      acc.className = "field";
      acc.append(label("帳號"));
      const btn = document.createElement("button");
      btn.type = "button";
      if (st.login === "waiting") {
        btn.textContent = "取消登入";
        btn.addEventListener("click", () => send({ type: "login" }));
        const code = document.createElement("p");
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
      modal.append(selectField("模型", st.models, snap.header.model, (id) => send({ type: "set_model", id })));
      modal.append(selectField("思考強度", st.efforts, effortId(st), (id) => send({ type: "set_effort", id })));
      const search = document.createElement("button");
      search.type = "button";
      search.textContent = st.web_search ? "搜尋：開" : "搜尋：關";
      search.addEventListener("click", () => send({ type: "toggle_search" }));
      modal.append(search);
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
    overlay.append(modal);
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
  function selectField(title, pairs, current, onChange) {
    const wrap = document.createElement("div");
    wrap.className = "field";
    wrap.append(label(title));
    const sel = document.createElement("select");
    (pairs || []).forEach(([id, name]) => {
      const o = document.createElement("option");
      o.value = id;
      o.textContent = name || id;
      if (id === current) o.selected = true;
      sel.append(o);
    });
    sel.addEventListener("change", () => onChange(sel.value));
    wrap.append(sel);
    return wrap;
  }

  $("new-chat").addEventListener("click", () => send({ type: "new_chat" }));
  $("gear").addEventListener("click", () => send({ type: "open_settings" }));
  $("mode-queue").addEventListener("click", () => send({ type: "set_send_mode", mode: "queue" }));
  $("mode-insert").addEventListener("click", () => send({ type: "set_send_mode", mode: "insert" }));
  $("paste-image").addEventListener("click", () => send({ type: "paste_image" }));
  $("interrupt").addEventListener("click", () => send({ type: "interrupt" }));
  overlay.addEventListener("click", (e) => {
    if (e.target === overlay) {
      if (snap && snap.image_view) send({ type: "close_image" });
      else if (snap && snap.settings) send({ type: "close_settings" });
      else if (snap && snap.inspector) send({ type: "close_inspector" });
      else if (snap && snap.skill_view) send({ type: "close_skill" });
      else if (snap && snap.tool_panel) send({ type: "close_tool" });
    }
  });

  document.addEventListener("paste", (e) => {
    const t = e.clipboardData && e.clipboardData.getData("text");
    if (t && document.activeElement !== composer) {
      send({ type: "paste_text", text: t });
    }
  });

  function connect() {
    if (!token) {
      $("header-left").textContent = "缺少存取 token，請從 TUI 開啟的網址進入";
      return;
    }
    const proto = location.protocol === "https:" ? "wss" : "ws";
    ws = new WebSocket(`${proto}://${location.host}/ws?t=${encodeURIComponent(token)}`);
    ws.onmessage = (ev) => {
      let msg;
      try { msg = JSON.parse(ev.data); } catch { return; }
      if (msg.type === "hello" || msg.type === "snapshot") applySnapshot(msg.snapshot);
    };
    ws.onclose = () => setTimeout(connect, 800);
  }
  connect();
})();
