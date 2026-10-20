"use strict";

// Web client for the acp-mux gateway. It speaks ACP JSON-RPC over a WebSocket
// as an observer: it can list existing sessions, follow them, send messages,
// answer permission requests and cancel turns, but not start sessions.

const META_KEY = "zed.dev/acpMux";
const TURN_STATE_METHOD = "_zed.dev/acpMux/turn_state";
const LIST_REFRESH_MS = 5000;

const STATE_LABELS = {
  running: "运行中",
  waiting: "等待授权",
  idle: "空闲",
  dead: "已退出",
};
const TOOL_STATUS_ICONS = { pending: "○", in_progress: "◐", completed: "✓", failed: "✕" };
const TOOL_KIND_LABELS = {
  read: "读取",
  edit: "编辑",
  delete: "删除",
  move: "移动",
  search: "搜索",
  execute: "执行",
  think: "思考",
  fetch: "获取",
  switch_mode: "切换模式",
  other: "其他",
};
const STOP_REASON_LABELS = {
  end_turn: "已完成",
  cancelled: "已取消",
  max_tokens: "达到 token 上限",
  max_turn_requests: "达到轮次上限",
  refusal: "agent 拒绝了请求",
};

const $ = (id) => document.getElementById(id);

function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

// ---------------------------------------------------------------------------
// Connection

const connection = {
  socket: null,
  nextId: 1,
  pending: new Map(),
  connected: false,
  retryDelay: 1000,
  retryTimer: null,
};

function connect() {
  clearTimeout(connection.retryTimer);
  const scheme = location.protocol === "https:" ? "wss://" : "ws://";
  const socket = new WebSocket(`${scheme}${location.host}/api/v1/acp`);
  connection.socket = socket;
  setStatus("连接中", "offline");

  socket.addEventListener("open", async () => {
    connection.connected = true;
    connection.retryDelay = 1000;
    try {
      await request("initialize", {
        protocolVersion: 1,
        clientCapabilities: {},
        clientInfo: { name: "acp-mux-web", version: "1" },
      });
      route();
    } catch (error) {
      showBanner(`初始化失败：${error.message}`);
    }
  });
  socket.addEventListener("message", (event) => {
    let message;
    try {
      message = JSON.parse(event.data);
    } catch {
      return;
    }
    handleMessage(message);
  });
  socket.addEventListener("close", () => {
    if (connection.socket !== socket) return;
    connection.connected = false;
    for (const { reject } of connection.pending.values()) reject(new Error("连接已断开"));
    connection.pending.clear();
    setStatus("已断开", "offline");
    scheduleReconnect();
  });
}

async function scheduleReconnect() {
  // A rejected handshake looks like any other closed connection, so ask the
  // gateway whether the login is still valid.
  const health = await fetchHealth();
  if (health && !health.authenticated) {
    showLogin();
    return;
  }
  connection.retryTimer = setTimeout(connect, connection.retryDelay);
  connection.retryDelay = Math.min(connection.retryDelay * 2, 30000);
}

function send(message) {
  if (!connection.connected) throw new Error("未连接");
  connection.socket.send(JSON.stringify({ jsonrpc: "2.0", ...message }));
}

function request(method, params) {
  return new Promise((resolve, reject) => {
    const id = connection.nextId++;
    connection.pending.set(id, { resolve, reject });
    try {
      send({ id, method, params });
    } catch (error) {
      connection.pending.delete(id);
      reject(error);
    }
  });
}

function respond(id, result) {
  send({ id, result });
}

function handleMessage(message) {
  if (message.method === undefined) {
    const pending = connection.pending.get(message.id);
    if (!pending) return;
    connection.pending.delete(message.id);
    if (message.error) {
      pending.reject(new Error(message.error.message || "请求失败"));
    } else {
      pending.resolve(message.result);
    }
    return;
  }
  const params = message.params || {};
  if (message.id !== undefined) {
    handleAgentRequest(message);
    return;
  }
  switch (message.method) {
    case "session/update":
      if (session && params.sessionId === session.id) applyUpdate(params.update || {});
      break;
    case TURN_STATE_METHOD:
      if (session && params.sessionId === session.id) {
        setRunning(params.state === "running");
        if (params.state === "idle" && params.stopReason && params.stopReason !== "end_turn") {
          showBanner(`本轮${STOP_REASON_LABELS[params.stopReason] || "出错"}`);
        }
      }
      break;
    case "$/cancel_request":
      removeRequest(params.requestId);
      break;
  }
}

async function fetchHealth() {
  try {
    const response = await fetch("/api/v1/health", { credentials: "same-origin" });
    return await response.json();
  } catch {
    return null;
  }
}

// ---------------------------------------------------------------------------
// Views and routing

function show(view) {
  for (const id of ["login-view", "list-view", "session-view"]) {
    $(id).classList.toggle("hidden", id !== view);
  }
  $("back").classList.toggle("hidden", view !== "session-view");
}

function setStatus(text, kind) {
  const status = $("status");
  status.textContent = text;
  status.className = `chip ${kind || ""}`;
}

function setTitle(title, subtitle) {
  $("title").textContent = title;
  $("subtitle").textContent = subtitle || "";
  document.title = title;
}

function showBanner(text) {
  const entries = $("entries");
  const banner = element("p", "banner", text);
  if (!$("session-view").classList.contains("hidden")) {
    entries.appendChild(banner);
    scrollToBottom(true);
  } else {
    $("sessions").prepend(banner);
  }
}

function route() {
  const match = location.hash.match(/^#\/session\/(.+)$/);
  if (match) {
    openSession(decodeURIComponent(match[1]));
  } else {
    openList();
  }
}

window.addEventListener("hashchange", () => {
  if (connection.connected) route();
});

$("back").addEventListener("click", () => {
  location.hash = "#/";
});

// ---------------------------------------------------------------------------
// Login

function showLogin() {
  closeSession();
  show("login-view");
  setTitle("Zed Agent");
  setStatus("未登录", "offline");
  $("token").focus();
}

$("login-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  $("login-error").textContent = "";
  try {
    const response = await fetch("/api/v1/login", {
      method: "POST",
      credentials: "same-origin",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ token: $("token").value.trim() }),
    });
    if (!response.ok) {
      const body = await response.json().catch(() => ({}));
      $("login-error").textContent = response.status === 429 ? "尝试次数过多，请稍后再试" : body.error || "登录失败";
      return;
    }
    $("token").value = "";
    connect();
  } catch (error) {
    $("login-error").textContent = error.message;
  }
});

$("logout").addEventListener("click", async () => {
  await fetch("/api/v1/logout", { method: "POST", credentials: "same-origin" }).catch(() => {});
  connection.socket?.close();
  showLogin();
});

// ---------------------------------------------------------------------------
// Session list

let listTimer = null;
const knownSessions = new Map();

function openList() {
  closeSession();
  show("list-view");
  setTitle("会话");
  setStatus("已连接", "");
  refreshList();
}

async function refreshList() {
  clearTimeout(listTimer);
  if ($("list-view").classList.contains("hidden") || !connection.connected) return;
  try {
    const result = await request("session/list", {});
    renderList(result.sessions || []);
  } catch (error) {
    showBanner(`获取会话列表失败：${error.message}`);
  }
  listTimer = setTimeout(refreshList, LIST_REFRESH_MS);
}

function sessionMeta(entry) {
  return (entry._meta && entry._meta[META_KEY]) || {};
}

function stateOrder(state) {
  return { waiting: 0, running: 1, idle: 2, dead: 3 }[state] ?? 2;
}

function relativeTime(timestamp) {
  if (!timestamp) return "";
  const seconds = (Date.now() - Date.parse(timestamp)) / 1000;
  if (seconds < 60) return "刚刚";
  if (seconds < 3600) return `${Math.floor(seconds / 60)} 分钟前`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)} 小时前`;
  return new Date(timestamp).toLocaleString();
}

function renderList(sessions) {
  const container = $("sessions");
  container.replaceChildren();
  $("sessions-empty").classList.toggle("hidden", sessions.length > 0);
  knownSessions.clear();

  const groups = new Map();
  for (const entry of sessions) {
    knownSessions.set(entry.sessionId, entry);
    const agent = sessionMeta(entry).agent || "agent";
    if (!groups.has(agent)) groups.set(agent, []);
    groups.get(agent).push(entry);
  }
  for (const [agent, entries] of groups) {
    container.appendChild(element("div", "group-title", agent));
    entries.sort(
      (a, b) =>
        stateOrder(sessionMeta(a).state) - stateOrder(sessionMeta(b).state) ||
        Date.parse(b.updatedAt || 0) - Date.parse(a.updatedAt || 0),
    );
    for (const entry of entries) {
      const meta = sessionMeta(entry);
      const button = element("button", "card session-item");
      button.type = "button";
      const row = element("div", "row");
      row.appendChild(element("span", "name", entry.title || entry.cwd || entry.sessionId));
      const state = meta.state || "idle";
      row.appendChild(element("span", `chip ${state}`, STATE_LABELS[state] || state));
      button.appendChild(row);
      const observers = meta.observers ? ` · ${meta.observers} 处在看` : "";
      button.appendChild(element("div", "meta", `${entry.cwd || ""} · ${relativeTime(entry.updatedAt)}${observers}`));
      button.addEventListener("click", () => {
        location.hash = `#/session/${encodeURIComponent(entry.sessionId)}`;
      });
      container.appendChild(button);
    }
  }
}

// ---------------------------------------------------------------------------
// Session view

let session = null;

function newSessionState(id) {
  return {
    id,
    entries: [],
    toolCalls: new Map(),
    plan: null,
    modes: null,
    configOptions: null,
    running: false,
    dead: false,
    requests: new Map(),
    renderScheduled: false,
  };
}

async function openSession(id) {
  closeSession();
  clearTimeout(listTimer);
  session = newSessionState(id);
  const known = knownSessions.get(id);
  const meta = known ? sessionMeta(known) : {};
  session.dead = meta.state === "dead";
  show("session-view");
  setTitle(known?.title || known?.cwd || "会话", known ? `${meta.agent || ""} · ${known.cwd || ""}` : "");
  $("entries").replaceChildren();
  $("requests").replaceChildren();
  $("plan").classList.add("hidden");
  $("session-controls").classList.add("hidden");
  setRunning(false);
  updateComposer();

  const current = session;
  try {
    const result = await request("session/load", { sessionId: id, cwd: known?.cwd || "/", mcpServers: [] });
    if (session !== current) return;
    session.modes = result.modes || null;
    session.configOptions = result.configOptions || null;
    renderControls();
    scheduleRender();
    scrollToBottom(true);
  } catch (error) {
    if (session !== current) return;
    showBanner(`打开会话失败：${error.message}`);
    session.dead = true;
    updateComposer();
  }
}

function closeSession() {
  if (!session) return;
  const id = session.id;
  session = null;
  if (connection.connected) request("session/close", { sessionId: id }).catch(() => {});
}

function setRunning(running) {
  if (!session) return;
  session.running = running;
  if (session.dead) {
    setStatus("已退出", "dead");
  } else if (session.requests.size > 0) {
    setStatus("等待授权", "waiting");
  } else {
    setStatus(running ? "运行中" : "空闲", running ? "running" : "");
  }
  updateComposer();
}

function updateComposer() {
  if (!session) return;
  $("stop").classList.toggle("hidden", !session.running);
  $("composer").classList.toggle("hidden", session.dead);
  $("session-note").classList.toggle("hidden", !session.dead);
  $("session-note").textContent = session.dead ? "agent 进程已退出，只能查看。" : "";
}

// Applies one `session/update` with the same merge rules as the daemon's
// snapshot, so replayed history and live updates look the same.
function applyUpdate(update) {
  const kind = update.sessionUpdate;
  const content = update.content;
  switch (kind) {
    case "user_message_chunk":
    case "agent_message_chunk":
    case "agent_thought_chunk": {
      const role = { user_message_chunk: "user", agent_message_chunk: "agent", agent_thought_chunk: "thought" }[kind];
      const last = session.entries[session.entries.length - 1];
      const messageId = update.messageId ?? null;
      if (last && last.kind === role && last.messageId === messageId) {
        appendContent(last, content);
      } else {
        const entry = { kind: role, messageId, text: "", extras: [], dirty: true };
        appendContent(entry, content);
        session.entries.push(entry);
      }
      break;
    }
    case "tool_call":
    case "tool_call_update": {
      const id = update.toolCallId;
      let entry = session.toolCalls.get(id);
      if (!entry) {
        entry = { kind: "tool", data: { toolCallId: id }, dirty: true };
        session.toolCalls.set(id, entry);
        session.entries.push(entry);
      }
      for (const [key, value] of Object.entries(update)) {
        if (key !== "sessionUpdate" && value !== null && value !== undefined) entry.data[key] = value;
      }
      entry.dirty = true;
      break;
    }
    case "plan":
      session.plan = update.entries || [];
      renderPlan();
      break;
    case "current_mode_update":
      if (session.modes) session.modes.currentModeId = update.currentModeId;
      renderControls();
      break;
    case "config_option_update":
      session.configOptions = update.configOptions || null;
      renderControls();
      break;
    case "session_info_update":
      if (update.title) setTitle(update.title, $("subtitle").textContent);
      break;
    default:
      return;
  }
  scheduleRender();
}

function appendContent(entry, content) {
  if (!content) return;
  if (content.type === "text") {
    entry.text += content.text || "";
  } else if (content.type === "resource_link") {
    entry.extras.push(`🔗 ${content.name || content.uri}`);
  } else if (content.type === "resource") {
    entry.extras.push(`📎 ${content.resource?.uri || "资源"}`);
  } else if (content.type === "image") {
    entry.extras.push("🖼 图片");
  } else {
    entry.extras.push(`[${content.type}]`);
  }
  entry.dirty = true;
}

function scheduleRender() {
  if (!session || session.renderScheduled) return;
  session.renderScheduled = true;
  setTimeout(() => {
    if (!session) return;
    session.renderScheduled = false;
    renderEntries();
  }, 80);
}

function isNearBottom() {
  return window.innerHeight + window.scrollY >= document.body.scrollHeight - 120;
}

function scrollToBottom(force) {
  if (force || isNearBottom()) window.scrollTo(0, document.body.scrollHeight);
}

function renderEntries() {
  const stick = isNearBottom();
  const container = $("entries");
  for (const entry of session.entries) {
    if (!entry.node) {
      entry.node = element("div", `entry ${entry.kind}`);
      container.appendChild(entry.node);
      entry.dirty = true;
    }
    if (!entry.dirty) continue;
    entry.dirty = false;
    if (entry.kind === "tool") {
      renderToolCall(entry);
    } else if (entry.kind === "thought") {
      const details = entry.node.querySelector("details") || element("details", "thought");
      details.replaceChildren(element("summary", "", "💭 思考"), renderMarkdown(entry.text));
      entry.node.replaceChildren(details);
    } else {
      entry.node.replaceChildren(renderMarkdown(entry.text));
      for (const extra of entry.extras) entry.node.appendChild(element("div", "muted", extra));
    }
  }
  if (stick) scrollToBottom(true);
}

function renderToolCall(entry) {
  const data = entry.data;
  const details = entry.node.querySelector("details") || element("details", "tool");
  const wasOpen = details.open;
  const summary = element("summary");
  const head = element("div", "tool-head");
  const status = data.status || "pending";
  head.appendChild(element("span", `status-icon ${status}`, TOOL_STATUS_ICONS[status] || "○"));
  head.appendChild(element("span", "tool-title", data.title || "工具调用"));
  if (data.kind) head.appendChild(element("span", "tool-kind", TOOL_KIND_LABELS[data.kind] || data.kind));
  summary.appendChild(head);

  const body = element("div", "tool-body");
  if (Array.isArray(data.locations) && data.locations.length) {
    const locations = element("div", "locations");
    locations.textContent = data.locations.map((location) => location.path + (location.line ? `:${location.line}` : "")).join("\n");
    body.appendChild(locations);
  }
  let hasDiff = false;
  for (const item of data.content || []) {
    if (item.type === "diff") {
      hasDiff = true;
      body.appendChild(renderDiff(item));
    } else if (item.type === "content" && item.content) {
      if (item.content.type === "text") {
        body.appendChild(renderMarkdown(item.content.text || ""));
      } else {
        body.appendChild(element("div", "muted", `[${item.content.type}]`));
      }
    } else if (item.type === "terminal") {
      body.appendChild(element("div", "muted", "终端输出不在此显示"));
    }
  }
  details.replaceChildren(summary, body);
  // Edits are the part worth reviewing on a phone, so show them unfolded.
  details.open = wasOpen || (hasDiff && !entry.opened);
  if (hasDiff) entry.opened = true;
  entry.node.replaceChildren(details);
}

// ---------------------------------------------------------------------------
// Diffs

const MAX_DIFF_CELLS = 4_000_000;
const DIFF_CONTEXT = 3;

function diffLines(oldLines, newLines) {
  const rows = oldLines.length;
  const columns = newLines.length;
  if (rows * columns > MAX_DIFF_CELLS) {
    return [
      ...oldLines.map((line) => ({ type: "removed", line })),
      ...newLines.map((line) => ({ type: "added", line })),
    ];
  }
  // Longest common subsequence, filled from the end so the walk goes forward.
  const width = columns + 1;
  const lengths = new Uint32Array((rows + 1) * width);
  for (let row = rows - 1; row >= 0; row--) {
    for (let column = columns - 1; column >= 0; column--) {
      lengths[row * width + column] =
        oldLines[row] === newLines[column]
          ? lengths[(row + 1) * width + column + 1] + 1
          : Math.max(lengths[(row + 1) * width + column], lengths[row * width + column + 1]);
    }
  }
  const result = [];
  let row = 0;
  let column = 0;
  while (row < rows && column < columns) {
    if (oldLines[row] === newLines[column]) {
      result.push({ type: "same", line: oldLines[row] });
      row++;
      column++;
    } else if (lengths[(row + 1) * width + column] >= lengths[row * width + column + 1]) {
      result.push({ type: "removed", line: oldLines[row++] });
    } else {
      result.push({ type: "added", line: newLines[column++] });
    }
  }
  while (row < rows) result.push({ type: "removed", line: oldLines[row++] });
  while (column < columns) result.push({ type: "added", line: newLines[column++] });
  return result;
}

function renderDiff(item) {
  const container = element("div", "diff");
  container.appendChild(element("div", "diff-path", item.path || ""));
  const lines = element("div", "diff-lines");
  const oldLines = item.oldText ? item.oldText.split("\n") : [];
  const newLines = (item.newText || "").split("\n");
  const changes = diffLines(oldLines, newLines);
  const visible = changes.map(() => false);
  changes.forEach((change, index) => {
    if (change.type === "same") return;
    for (let offset = -DIFF_CONTEXT; offset <= DIFF_CONTEXT; offset++) {
      if (changes[index + offset]) visible[index + offset] = true;
    }
  });
  let skipped = 0;
  const flushSkipped = () => {
    if (skipped > 0) lines.appendChild(element("div", "skipped", `… ${skipped} 行未改动`));
    skipped = 0;
  };
  changes.forEach((change, index) => {
    if (!visible[index]) {
      skipped++;
      return;
    }
    flushSkipped();
    const prefix = { same: " ", added: "+", removed: "-" }[change.type];
    lines.appendChild(element("div", change.type, `${prefix} ${change.line}`));
  });
  flushSkipped();
  container.appendChild(lines);
  return container;
}

// ---------------------------------------------------------------------------
// Markdown. Builds DOM nodes instead of HTML strings, so agent output can never
// inject markup.

function renderMarkdown(text) {
  const fragment = document.createDocumentFragment();
  const lines = text.split("\n");
  let index = 0;
  let paragraph = [];
  const flushParagraph = () => {
    if (paragraph.length) {
      const node = element("p");
      appendInline(node, paragraph.join("\n"));
      fragment.appendChild(node);
      paragraph = [];
    }
  };

  while (index < lines.length) {
    const line = lines[index];
    const fence = line.match(/^\s*(```|~~~)/);
    if (fence) {
      flushParagraph();
      const code = [];
      index++;
      while (index < lines.length && !lines[index].trim().startsWith(fence[1])) code.push(lines[index++]);
      index++;
      const pre = element("pre");
      pre.appendChild(element("code", "", code.join("\n")));
      fragment.appendChild(pre);
      continue;
    }
    const heading = line.match(/^(#{1,6})\s+(.*)$/);
    if (heading) {
      flushParagraph();
      const node = element(`h${Math.min(heading[1].length + 2, 6)}`);
      appendInline(node, heading[2]);
      fragment.appendChild(node);
      index++;
      continue;
    }
    const listItem = line.match(/^\s*([-*+]|\d+[.)])\s+(.*)$/);
    if (listItem) {
      flushParagraph();
      const ordered = /\d/.test(listItem[1]);
      const list = element(ordered ? "ol" : "ul");
      while (index < lines.length) {
        const item = lines[index].match(/^\s*([-*+]|\d+[.)])\s+(.*)$/);
        if (!item || /\d/.test(item[1]) !== ordered) break;
        const node = element("li");
        appendInline(node, item[2]);
        list.appendChild(node);
        index++;
      }
      fragment.appendChild(list);
      continue;
    }
    if (line.startsWith(">")) {
      flushParagraph();
      const quote = [];
      while (index < lines.length && lines[index].startsWith(">")) quote.push(lines[index++].replace(/^>\s?/, ""));
      const node = element("blockquote");
      appendInline(node, quote.join("\n"));
      fragment.appendChild(node);
      continue;
    }
    if (/^\s*([-*_])(\s*\1){2,}\s*$/.test(line)) {
      flushParagraph();
      fragment.appendChild(element("hr"));
      index++;
      continue;
    }
    if (line.trim() === "") {
      flushParagraph();
    } else {
      paragraph.push(line);
    }
    index++;
  }
  flushParagraph();
  return fragment;
}

const INLINE_PATTERN = /(`+)([\s\S]*?)\1|\*\*([\s\S]+?)\*\*|__([\s\S]+?)__|\*([^*\n]+)\*|_([^_\n]+)_|\[([^\]]+)\]\(([^)\s]+)\)/g;

function appendInline(parent, text) {
  let last = 0;
  for (const match of text.matchAll(INLINE_PATTERN)) {
    appendText(parent, text.slice(last, match.index));
    last = match.index + match[0].length;
    if (match[1]) {
      parent.appendChild(element("code", "", match[2]));
    } else if (match[3] || match[4]) {
      const node = element("strong");
      appendInline(node, match[3] || match[4]);
      parent.appendChild(node);
    } else if (match[5] || match[6]) {
      const node = element("em");
      appendInline(node, match[5] || match[6]);
      parent.appendChild(node);
    } else if (match[7]) {
      if (/^https?:\/\//i.test(match[8])) {
        const link = element("a", "", match[7]);
        link.href = match[8];
        link.target = "_blank";
        link.rel = "noopener noreferrer";
        parent.appendChild(link);
      } else {
        appendText(parent, match[7]);
      }
    }
  }
  appendText(parent, text.slice(last));
}

function appendText(parent, text) {
  if (!text) return;
  const parts = text.split("\n");
  parts.forEach((part, index) => {
    if (index > 0) parent.appendChild(element("br"));
    if (part) parent.appendChild(document.createTextNode(part));
  });
}

// ---------------------------------------------------------------------------
// Plan, modes and config options

function renderPlan() {
  const container = $("plan");
  const entries = session.plan || [];
  container.classList.toggle("hidden", entries.length === 0);
  const list = element("ol");
  for (const entry of entries) {
    const item = element("li", entry.status || "");
    appendInline(item, entry.content || "");
    list.appendChild(item);
  }
  container.replaceChildren(element("strong", "", "计划"), list);
}

function renderControls() {
  if (!session) return;
  const container = $("session-controls");
  container.replaceChildren();
  const modes = session.modes;
  if (modes && Array.isArray(modes.availableModes) && modes.availableModes.length > 1) {
    const select = element("select");
    for (const mode of modes.availableModes) {
      const option = element("option", "", mode.name || mode.id);
      option.value = mode.id;
      option.selected = mode.id === modes.currentModeId;
      select.appendChild(option);
    }
    select.addEventListener("change", async () => {
      try {
        await request("session/set_mode", { sessionId: session.id, modeId: select.value });
        session.modes.currentModeId = select.value;
      } catch (error) {
        showBanner(`切换模式失败：${error.message}`);
        renderControls();
      }
    });
    container.appendChild(labelled("模式", select));
  }
  for (const option of session.configOptions || []) {
    const control = configControl(option);
    if (control) container.appendChild(labelled(option.name || option.id, control));
  }
  container.classList.toggle("hidden", container.childElementCount === 0);
}

function labelled(text, control) {
  const label = element("label", "", text);
  label.appendChild(control);
  return label;
}

function configControl(option) {
  const setOption = async (value) => {
    try {
      const result = await request("session/set_config_option", {
        sessionId: session.id,
        configId: option.id,
        ...value,
      });
      if (result && result.configOptions) session.configOptions = result.configOptions;
    } catch (error) {
      showBanner(`修改设置失败：${error.message}`);
    }
    renderControls();
  };
  if (option.type === "boolean") {
    const checkbox = element("input");
    checkbox.type = "checkbox";
    checkbox.checked = option.currentValue === true;
    checkbox.addEventListener("change", () => setOption({ type: "boolean", value: checkbox.checked }));
    return checkbox;
  }
  if (option.type === "select") {
    const select = element("select");
    const values = [];
    for (const item of option.options || []) {
      if (Array.isArray(item.options)) {
        values.push(...item.options);
      } else {
        values.push(item);
      }
    }
    for (const value of values) {
      const node = element("option", "", value.name || value.value);
      node.value = value.value;
      node.selected = value.value === option.currentValue;
      select.appendChild(node);
    }
    select.addEventListener("change", () => setOption({ value: select.value }));
    return select;
  }
  return null;
}

// ---------------------------------------------------------------------------
// Requests from the agent: permissions and elicitations

function handleAgentRequest(message) {
  const params = message.params || {};
  if (!session || (params.sessionId && params.sessionId !== session.id)) {
    send({ id: message.id, error: { code: -32603, message: "The session is not open in this client" } });
    return;
  }
  if (message.method === "session/request_permission") {
    session.requests.set(message.id, renderPermission(message));
  } else if (message.method === "elicitation/create") {
    session.requests.set(message.id, renderElicitation(message));
  } else {
    send({ id: message.id, error: { code: -32601, message: "Method not found" } });
    return;
  }
  setRunning(session.running);
  scrollToBottom(true);
}

function removeRequest(id) {
  if (!session) return;
  const node = session.requests.get(id);
  if (!node) return;
  node.remove();
  session.requests.delete(id);
  setRunning(session.running);
}

function answer(id, result) {
  try {
    respond(id, result);
  } catch (error) {
    showBanner(`回答失败：${error.message}`);
    return;
  }
  removeRequest(id);
}

function renderPermission(message) {
  const params = message.params;
  const card = element("div", "card request");
  const title = params.toolCall?.title || "agent 请求授权";
  card.appendChild(element("p", "question", `🔐 ${title}`));
  const options = element("div", "options");
  for (const option of params.options || []) {
    const style = option.kind?.startsWith("allow") ? "success" : option.kind?.startsWith("reject") ? "danger" : "";
    const button = element("button", style, option.name || option.optionId);
    button.type = "button";
    button.addEventListener("click", () => {
      answer(message.id, { outcome: { outcome: "selected", optionId: option.optionId } });
    });
    options.appendChild(button);
  }
  card.appendChild(options);
  $("requests").appendChild(card);
  return card;
}

function renderElicitation(message) {
  const params = message.params;
  const card = element("div", "card request");
  card.appendChild(element("p", "question", `❓ ${params.message || "agent 需要你的输入"}`));

  if (params.mode === "url") {
    const options = element("div", "options");
    const open = element("button", "primary", "打开链接");
    open.type = "button";
    open.addEventListener("click", () => {
      window.open(params.url, "_blank", "noopener,noreferrer");
      answer(message.id, { action: "accept" });
    });
    const decline = element("button", "danger", "拒绝");
    decline.type = "button";
    decline.addEventListener("click", () => answer(message.id, { action: "decline" }));
    options.append(open, decline);
    card.appendChild(options);
    $("requests").appendChild(card);
    return card;
  }

  const schema = params.requestedSchema || {};
  const required = new Set(schema.required || []);
  const form = element("form");
  const fields = [];
  for (const [name, property] of Object.entries(schema.properties || {})) {
    const label = element("label", "", property.title || name);
    let input;
    const choices = property.enum || property.oneOf?.map((choice) => choice.const) || property.anyOf?.map((choice) => choice.const);
    if (property.type === "boolean") {
      input = element("input");
      input.type = "checkbox";
      input.checked = property.default === true;
    } else if (choices) {
      input = element("select");
      choices.forEach((choice, index) => {
        const titles = property.enumNames || property.oneOf || property.anyOf;
        const text = titles ? titles[index]?.title || titles[index] : choice;
        const option = element("option", "", String(text ?? choice));
        option.value = String(choice);
        option.selected = choice === property.default;
        input.appendChild(option);
      });
    } else {
      input = element("input");
      input.type = property.type === "number" || property.type === "integer" ? "number" : "text";
      if (property.type === "integer") input.step = "1";
      if (property.default !== undefined) input.value = String(property.default);
      input.required = required.has(name);
    }
    if (property.description) label.appendChild(element("span", "muted", property.description));
    label.appendChild(input);
    form.appendChild(label);
    fields.push({ name, property, input });
  }
  const buttons = element("div", "options");
  const submit = element("button", "primary", "提交");
  submit.type = "submit";
  const decline = element("button", "danger", "拒绝");
  decline.type = "button";
  decline.addEventListener("click", () => answer(message.id, { action: "decline" }));
  const cancel = element("button", "", "取消");
  cancel.type = "button";
  cancel.addEventListener("click", () => answer(message.id, { action: "cancel" }));
  buttons.append(submit, decline, cancel);
  form.appendChild(buttons);
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    const content = {};
    for (const { name, property, input } of fields) {
      if (property.type === "boolean") {
        content[name] = input.checked;
      } else if (input.value === "" && !required.has(name)) {
        continue;
      } else if (property.type === "number" || property.type === "integer") {
        content[name] = Number(input.value);
      } else {
        content[name] = input.value;
      }
    }
    answer(message.id, { action: "accept", content });
  });
  card.appendChild(form);
  $("requests").appendChild(card);
  return card;
}

// ---------------------------------------------------------------------------
// Composer

const textarea = $("message");

textarea.addEventListener("input", () => {
  textarea.style.height = "auto";
  textarea.style.height = `${textarea.scrollHeight}px`;
});

textarea.addEventListener("keydown", (event) => {
  if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
    event.preventDefault();
    $("composer").requestSubmit();
  }
});

$("composer").addEventListener("submit", async (event) => {
  event.preventDefault();
  if (!session) return;
  const text = textarea.value.trim();
  if (!text) return;
  if (session.running && !confirm("agent 正在执行，发送会中断当前轮次。继续？")) return;

  const current = session;
  textarea.value = "";
  textarea.style.height = "auto";
  applyUpdate({ sessionUpdate: "user_message_chunk", content: { type: "text", text } });
  // Keeps the next agent reply out of the user's message.
  session.entries[session.entries.length - 1].messageId = `local-${Date.now()}`;
  setRunning(true);
  scrollToBottom(true);
  try {
    const result = await request("session/prompt", {
      sessionId: current.id,
      prompt: [{ type: "text", text }],
    });
    if (session !== current) return;
    if (result.stopReason && result.stopReason !== "end_turn") {
      showBanner(`本轮${STOP_REASON_LABELS[result.stopReason] || "出错"}`);
    }
  } catch (error) {
    if (session === current) showBanner(`发送失败：${error.message}`);
  }
  if (session === current) setRunning(false);
});

$("stop").addEventListener("click", () => {
  if (!session) return;
  try {
    send({ method: "session/cancel", params: { sessionId: session.id } });
  } catch (error) {
    showBanner(`停止失败：${error.message}`);
  }
});

// ---------------------------------------------------------------------------
// Start

if ("serviceWorker" in navigator) {
  navigator.serviceWorker.register("/sw.js").catch(() => {});
}

(async () => {
  const health = await fetchHealth();
  if (health && !health.authenticated) {
    showLogin();
  } else {
    connect();
  }
})();
