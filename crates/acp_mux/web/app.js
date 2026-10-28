"use strict";

// Web client for the acp-mux gateway. It speaks ACP JSON-RPC over a WebSocket
// as an observer: it can list existing sessions, follow them, send messages,
// answer permission requests and cancel turns, but not start sessions.

const META_KEY = "zed.dev/acpMux";
const TURN_STATE_METHOD = "_zed.dev/acpMux/turn_state";
const LIST_REFRESH_MS = 5000;
const WIDE = window.matchMedia("(min-width: 900px)");
const COARSE = window.matchMedia("(pointer: coarse)");

const STATE_LABELS = {
  running: "运行中",
  waiting: "等待授权",
  idle: "空闲",
  dead: "已退出",
};
const TOOL_ICONS = {
  read: "read",
  edit: "edit",
  delete: "delete",
  move: "move",
  search: "search",
  execute: "execute",
  think: "think",
  fetch: "fetch",
  switch_mode: "switch",
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
  if (text !== undefined && text !== null) node.textContent = text;
  return node;
}

function icon(name, className) {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("class", className ? `icon ${className}` : "icon");
  svg.setAttribute("aria-hidden", "true");
  const use = document.createElementNS("http://www.w3.org/2000/svg", "use");
  use.setAttribute("href", `#i-${name}`);
  svg.appendChild(use);
  return svg;
}

function stateMark(state, label) {
  const mark = element("span", `state ${state}`);
  if (state === "running" || state === "in_progress") mark.appendChild(icon("spinner"));
  else if (state === "completed") mark.appendChild(icon("check"));
  else if (state === "failed") mark.appendChild(icon("x"));
  else if (state === "pending") mark.appendChild(icon("pending"));
  else if (state === "dead") mark.appendChild(icon("x"));
  if (label) {
    mark.setAttribute("role", "img");
    mark.setAttribute("aria-label", label);
  } else {
    mark.setAttribute("aria-hidden", "true");
  }
  return mark;
}

// Long paths are cut from the left so the file name stays visible. The text sits
// in an isolated left-to-right span, otherwise the right-to-left container
// would move leading slashes to the end.
function pathElement(className, path) {
  const container = element("span", className);
  container.title = path;
  container.appendChild(element("span", "ltr", path));
  return container;
}

function basename(path) {
  const trimmed = (path || "").replace(/\/+$/, "");
  return trimmed.slice(trimmed.lastIndexOf("/") + 1) || trimmed || "/";
}

function dirname(path) {
  const trimmed = (path || "").replace(/\/+$/, "");
  const index = trimmed.lastIndexOf("/");
  return index > 0 ? trimmed.slice(0, index) : "";
}

function relativeTime(timestamp) {
  if (!timestamp) return "";
  const seconds = (Date.now() - Date.parse(timestamp)) / 1000;
  if (seconds < 45) return "刚刚";
  if (seconds < 3600) return `${Math.max(1, Math.round(seconds / 60))} 分钟前`;
  if (seconds < 86400) return `${Math.round(seconds / 3600)} 小时前`;
  if (seconds < 7 * 86400) return `${Math.round(seconds / 86400)} 天前`;
  return new Date(timestamp).toLocaleDateString();
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
  showApp();
  const scheme = location.protocol === "https:" ? "wss://" : "ws://";
  const socket = new WebSocket(`${scheme}${location.host}/api/v1/acp`);
  connection.socket = socket;
  setConnection("connecting");

  socket.addEventListener("open", async () => {
    connection.connected = true;
    connection.retryDelay = 1000;
    try {
      await request("initialize", {
        protocolVersion: 1,
        clientCapabilities: {},
        _meta: { [META_KEY]: { sessionLifecycle: true } },
        clientInfo: { name: "acp-mux-web", version: "2" },
      });
      setConnection("online");
      refreshList();
      route();
    } catch (error) {
      showListBanner(`初始化失败：${error.message}`);
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
    for (const { reject, timer } of connection.pending.values()) {
      clearTimeout(timer);
      reject(new Error("连接已断开"));
    }
    connection.pending.clear();
    // The daemon forgets this observer with the connection, so the open
    // session has to be loaded again after reconnecting.
    if (session) session.stale = true;
    setConnection("offline");
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
  const seconds = Math.round(connection.retryDelay / 1000);
  showListBanner(`连接已断开，${seconds} 秒后重试`);
  connection.retryTimer = setTimeout(connect, connection.retryDelay);
  connection.retryDelay = Math.min(connection.retryDelay * 2, 30000);
}

function send(message) {
  if (!connection.connected) throw new Error("未连接到服务器");
  connection.socket.send(JSON.stringify({ jsonrpc: "2.0", ...message }));
}

function request(method, params, timeoutMs = 0) {
  return new Promise((resolve, reject) => {
    const id = connection.nextId++;
    const timer = timeoutMs > 0 ? setTimeout(() => {
      connection.pending.delete(id);
      reject(new Error("请求超时，请重试"));
    }, timeoutMs) : null;
    connection.pending.set(id, { resolve, reject, timer });
    try {
      send({ id, method, params });
    } catch (error) {
      clearTimeout(timer);
      connection.pending.delete(id);
      reject(error);
    }
  });
}

function handleMessage(message) {
  if (message.method === undefined) {
    const pending = connection.pending.get(message.id);
    if (!pending) return;
    connection.pending.delete(message.id);
    clearTimeout(pending.timer);
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
    case "_zed.dev/acpMux/sessions_changed":
      if (params.kind === "deleted" && currentRoute() === params.sessionId) {
        closeSession();
        location.hash = "#/";
        route();
      }
      refreshList();
      break;
    case "session/update":
      if (session && params.sessionId === session.id) applyUpdate(params.update || {});
      break;
    case TURN_STATE_METHOD:
      if (session && params.sessionId === session.id) {
        setRunning(params.state === "running");
        if (params.state === "idle") reportStop(params.stopReason);
      }
      break;
    case "$/cancel_request":
      removeRequest(params.requestId);
      break;
  }
}

async function fetchHealth() {
  try {
    const response = await fetch("/api/v1/health", { credentials: "same-origin", cache: "no-store" });
    return await response.json();
  } catch {
    return null;
  }
}

function setConnection(state) {
  const node = $("connection");
  node.className = `connection ${state}`;
  node.textContent = { online: "已连接", connecting: "连接中", offline: "已断开" }[state];
  if (state === "online") $("list-banner").hidden = true;
  if (session) {
    updateStatus();
    updateComposer();
  }
}

function showListBanner(text) {
  const banner = $("list-banner");
  banner.replaceChildren(icon("alert"), element("span", "", text));
  banner.hidden = false;
}

// ---------------------------------------------------------------------------
// Layout and routing

function showApp() {
  $("login").hidden = true;
  $("app").classList.remove("logged-out");
}

function currentRoute() {
  const match = location.hash.match(/^#\/s\/(.+)$/);
  return match ? decodeURIComponent(match[1]) : null;
}

function route() {
  const id = currentRoute();
  $("app").dataset.view = id ? "thread" : "list";
  if (id) {
    if (!session || session.id !== id || session.stale) openSession(id);
  } else {
    closeSession();
    showThreadEmpty();
  }
  highlightSelection();
}

window.addEventListener("hashchange", () => {
  if (connection.connected) route();
  else $("app").dataset.view = currentRoute() ? "thread" : "list";
});

$("back").addEventListener("click", () => {
  location.hash = "#/";
});

// ---------------------------------------------------------------------------
// Login

function showLogin(message) {
  closeSession();
  clearTimeout(listTimer);
  $("app").classList.add("logged-out");
  $("login").hidden = false;
  setLoginError(message);
  setTimeout(() => $("token").focus(), 50);
}

function setLoginError(message) {
  $("login-error").hidden = !message;
  $("login-error").textContent = message || "";
}

$("token-toggle").addEventListener("click", () => {
  const input = $("token");
  const reveal = input.type === "password";
  input.type = reveal ? "text" : "password";
  $("token-toggle").setAttribute("aria-label", reveal ? "隐藏 token" : "显示 token");
  $("token-toggle").replaceChildren(icon(reveal ? "eye-off" : "eye"));
});

$("login-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  setLoginError(null);
  const submit = $("login-submit");
  submit.disabled = true;
  submit.textContent = "正在登录…";
  try {
    const response = await fetch("/api/v1/login", {
      method: "POST",
      credentials: "same-origin",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ token: $("token").value.trim() }),
    });
    if (!response.ok) {
      setLoginError(
        response.status === 429
          ? "失败次数太多，请一分钟后再试。"
          : response.status === 401
            ? "token 不正确。请复制服务器上 token 文件的完整内容。"
            : `登录失败（${response.status}）。`,
      );
      return;
    }
    // The login is a cookie. Browsers drop `Secure` cookies on plain HTTP,
    // which looks like a login that silently does nothing.
    const health = await fetchHealth();
    if (!health || !health.authenticated) {
      setLoginError(
        location.protocol === "http:"
          ? "token 正确，但浏览器没有保存登录 cookie。通过 http 访问时，需要在 acp_mux.json 的 gateway 中设置 \"cookie_secure\": false 并执行 acp-mux restart，或改用 https。"
          : "token 正确，但浏览器没有保存登录 cookie。请检查浏览器是否禁用了 cookie。",
      );
      return;
    }
    $("token").value = "";
    connect();
  } catch (error) {
    setLoginError(`无法连接到服务器：${error.message}`);
  } finally {
    submit.disabled = false;
    submit.textContent = "登录";
  }
});

$("logout").addEventListener("click", async () => {
  await fetch("/api/v1/logout", { method: "POST", credentials: "same-origin" }).catch(() => {});
  const socket = connection.socket;
  connection.socket = null;
  connection.connected = false;
  socket?.close();
  showLogin();
});

// ---------------------------------------------------------------------------
// Session list

let listTimer = null;
let listLoaded = false;
let listRevision = 0;
const knownSessions = new Map();

async function refreshList() {
  clearTimeout(listTimer);
  const revision = ++listRevision;
  if (!connection.connected) return;
  if (!listLoaded) renderListSkeleton();
  try {
    const result = await request("session/list", {}, 15000);
    if (revision !== listRevision) return;
    if (!Array.isArray(result.sessions) || result.nextCursor != null) throw new Error("会话列表不完整，请重试");
    const sync = result._meta?.[META_KEY]?.sessionSync;
    let deleted = [];
    if (sync != null) {
      if (sync.complete !== true || !Array.isArray(sync.deletedSessions) ||
          sync.deletedSessions.some(entry => typeof entry?.sessionId !== "string" || !entry.sessionId)) {
        throw new Error("会话同步数据不完整，请重试");
      }
      deleted = sync.deletedSessions.map(entry => entry.sessionId);
    }
    listLoaded = true;
    renderList(result.sessions.filter(entry => !deleted.includes(entry.sessionId)));
    if (deleted.includes(currentRoute())) {
      closeSession();
      location.hash = "#/";
      route();
      showListBanner("会话已在其他设备删除");
    }
  } catch (error) {
    if (revision !== listRevision) return;
    if (connection.connected) showListBanner(`获取会话失败：${error.message}`);
  }
  listTimer = setTimeout(refreshList, LIST_REFRESH_MS);
}

function sessionMeta(entry) {
  return (entry?._meta && entry._meta[META_KEY]) || {};
}

function renderListSkeleton() {
  const list = $("session-list");
  list.replaceChildren();
  for (let index = 0; index < 4; index++) {
    const row = element("div", "skeleton-row");
    row.appendChild(element("div", "skeleton skeleton-line"));
    const lines = element("div");
    lines.append(element("div", "skeleton skeleton-line"), element("div", "skeleton skeleton-line short"));
    row.appendChild(lines);
    list.appendChild(row);
  }
}

function renderList(entries) {
  const list = $("session-list");
  const scroll = list.scrollTop;
  list.replaceChildren();
  knownSessions.clear();
  for (const entry of entries) knownSessions.set(entry.sessionId, entry);

  if (entries.length === 0) {
    const empty = element("div", "list-empty");
    empty.appendChild(element("strong", "", "还没有会话"));
    empty.appendChild(element("span", "", "这里会列出 acp-mux 管理的 agent 会话："));
    const steps = element("ol");
    steps.append(
      element("li", "", "在服务器上开启 acp-mux"),
      element("li", "", "在 Zed 的远程项目里打开 agent 并发起任务"),
      element("li", "", "回到这里查看和操作"),
    );
    empty.appendChild(steps);
    list.appendChild(empty);
    updateDocumentTitle(0);
    return;
  }

  const waiting = entries.filter((entry) => sessionMeta(entry).state === "waiting");
  if (waiting.length) {
    list.appendChild(renderGroup("需要你处理", null, waiting, "attention-group", "shield"));
  }

  const projects = new Map();
  for (const entry of entries) {
    if (sessionMeta(entry).state === "waiting") continue;
    const key = entry.cwd || "";
    if (!projects.has(key)) projects.set(key, []);
    projects.get(key).push(entry);
  }
  const ordered = [...projects.entries()].sort(
    ([, a], [, b]) => Date.parse(b[0].updatedAt || 0) - Date.parse(a[0].updatedAt || 0),
  );
  for (const [cwd, group] of ordered) {
    list.appendChild(renderGroup(basename(cwd), dirname(cwd), group, "", "folder"));
  }
  list.scrollTop = scroll;
  updateDocumentTitle(waiting.length);
  highlightSelection();
  const open = session && knownSessions.get(session.id);
  if (open) {
    $("thread-meta").textContent = [sessionMeta(open).agent, open.cwd].filter(Boolean).join(" · ");
    $("thread-meta").title = open.cwd || "";
    if (open.title) $("thread-title").textContent = open.title;
  }
}

function renderGroup(name, path, entries, className, iconName) {
  const group = element("section", `group ${className}`);
  const head = element("h2", "group-head");
  head.appendChild(icon(iconName));
  head.appendChild(element("span", "", name));
  if (path) head.appendChild(pathElement("group-path", path));
  group.appendChild(head);
  for (const entry of entries) group.appendChild(renderRow(entry));
  return group;
}

function renderRow(entry) {
  const meta = sessionMeta(entry);
  const state = meta.state || "idle";
  const row = element("button", "session-row");
  row.type = "button";
  row.dataset.sessionId = entry.sessionId;
  row.appendChild(stateMark(state, STATE_LABELS[state]));

  const text = element("div");
  const title = element("div", entry.title ? "row-title" : "row-title untitled", entry.title || "未命名会话");
  const details = element("div", "row-meta");
  details.appendChild(element("span", "agent", meta.agent || "agent"));
  if (state === "running" || state === "waiting" || state === "dead") {
    details.appendChild(element("span", "", `· ${STATE_LABELS[state]}`));
  }
  if (meta.observers) {
    const watchers = element("span", "watchers");
    watchers.title = `${meta.observers} 处正在查看`;
    watchers.append(icon("eye"), document.createTextNode(String(meta.observers)));
    details.appendChild(watchers);
  }
  text.append(title, details);
  row.appendChild(text);
  row.appendChild(element("span", "row-time", relativeTime(entry.updatedAt)));
  row.addEventListener("click", () => {
    location.hash = `#/s/${encodeURIComponent(entry.sessionId)}`;
  });
  return row;
}

function highlightSelection() {
  const id = currentRoute();
  for (const row of document.querySelectorAll(".session-row")) {
    if (row.dataset.sessionId === id) row.setAttribute("aria-current", "page");
    else row.removeAttribute("aria-current");
  }
}

function updateDocumentTitle(waiting) {
  document.title = waiting ? `(${waiting}) Zed Agent` : "Zed Agent";
}

// ---------------------------------------------------------------------------
// Thread view

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
    loading: true,
    requests: new Map(),
    renderScheduled: false,
    unseen: false,
  };
}

function showThreadEmpty() {
  $("thread-empty").hidden = false;
  for (const id of ["thread-head", "stream", "dock", "jump"]) $(id).hidden = true;
}

async function openSession(id) {
  closeSession();
  session = newSessionState(id);
  const known = knownSessions.get(id);
  const meta = sessionMeta(known);
  session.dead = meta.state === "dead";

  $("thread-empty").hidden = true;
  $("thread-head").hidden = false;
  $("stream").hidden = false;
  $("dock").hidden = false;
  $("jump").hidden = true;
  $("attention").hidden = true;
  $("plan").hidden = true;
  $("controls").replaceChildren();
  setThreadHeading(known?.title, meta.agent, known?.cwd);
  renderStreamSkeleton();
  updateStatus();
  updateComposer();

  const current = session;
  try {
    const result = await request("session/load", { sessionId: id, cwd: known?.cwd || "/", mcpServers: [] });
    if (session !== current) return;
    session.loading = false;
    session.modes = result.modes || null;
    session.configOptions = result.configOptions || null;
    $("entries").querySelectorAll(".stream-skeleton").forEach((node) => node.remove());
    renderControls();
    renderEntries();
    scrollToBottom(true);
    if (!COARSE.matches && !session.dead) $("message").focus({ preventScroll: true });
  } catch (error) {
    if (session !== current) return;
    session.loading = false;
    $("entries").replaceChildren();
    appendNotice(`打开会话失败：${error.message}`, true);
    session.dead = true;
  }
  updateStatus();
  updateComposer();
}

function closeSession() {
  if (!session) return;
  const { id, stale } = session;
  session = null;
  $("entries").replaceChildren();
  $("generating").hidden = true;
  if (connection.connected && !stale) request("session/close", { sessionId: id }).catch(() => {});
}

function setThreadHeading(title, agent, cwd) {
  $("thread-title").textContent = title || "未命名会话";
  $("thread-meta").textContent = [agent, cwd].filter(Boolean).join(" · ");
  $("thread-meta").title = cwd || "";
}

function renderStreamSkeleton() {
  const entries = $("entries");
  entries.replaceChildren();
  for (const width of ["62%", "90%", "75%"]) {
    const block = element("div", "stream-skeleton entry");
    for (let line = 0; line < 3; line++) {
      const bar = element("div", "skeleton skeleton-line");
      bar.style.width = line === 2 ? "40%" : width;
      block.appendChild(bar);
    }
    entries.appendChild(block);
  }
}

function sessionState() {
  if (!session) return "idle";
  if (session.dead) return "dead";
  if (session.requests.size > 0) return "waiting";
  return session.running ? "running" : "idle";
}

function updateStatus() {
  if (!session) return;
  const pill = $("thread-status");
  if (!connection.connected) {
    pill.className = "status-pill offline";
    pill.replaceChildren(icon("alert"), element("span", "", "已断开"));
  } else if (session.loading) {
    pill.className = "status-pill";
    pill.replaceChildren(element("span", "", "加载中"));
  } else {
    const state = sessionState();
    pill.className = `status-pill ${state}`;
    pill.replaceChildren(stateMark(state), element("span", "", STATE_LABELS[state]));
  }
  $("generating").hidden = !(session.running && !session.loading && session.requests.size === 0);
  updateAttention();
}

function setRunning(running) {
  if (!session) return;
  session.running = running;
  updateStatus();
  updateComposer();
}

function reportStop(stopReason) {
  if (stopReason && stopReason !== "end_turn") {
    appendNotice(`本轮${STOP_REASON_LABELS[stopReason] || "出错"}`, stopReason !== "cancelled");
  }
}

function appendNotice(text, isError) {
  const notice = element("div", isError ? "notice error" : "notice");
  notice.append(icon(isError ? "alert" : "check"), element("span", "", text));
  $("entries").appendChild(notice);
  scrollToBottom(false);
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
        const entry = { kind: role, messageId, text: "", attachments: [], dirty: true };
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
      return;
    case "current_mode_update":
      if (session.modes) session.modes.currentModeId = update.currentModeId;
      renderControls();
      return;
    case "config_option_update":
      session.configOptions = update.configOptions || null;
      renderControls();
      return;
    case "session_info_update":
      if (update.title) $("thread-title").textContent = update.title;
      return;
    default:
      return;
  }
  session.unseen = true;
  scheduleRender();
}

function appendContent(entry, content) {
  if (!content) return;
  if (content.type === "text") {
    entry.text += content.text || "";
  } else if (content.type === "resource_link") {
    entry.attachments.push({ icon: "link", label: content.name || content.uri });
  } else if (content.type === "resource") {
    entry.attachments.push({ icon: "read", label: content.resource?.uri || "附加内容" });
  } else if (content.type === "image") {
    entry.attachments.push({ icon: "image", label: "图片" });
  } else {
    entry.attachments.push({ icon: "read", label: content.type });
  }
  entry.dirty = true;
}

function scheduleRender() {
  if (!session || session.renderScheduled || session.loading) return;
  session.renderScheduled = true;
  setTimeout(() => {
    if (!session) return;
    session.renderScheduled = false;
    renderEntries();
  }, 60);
}

function isNearBottom() {
  const stream = $("stream");
  return stream.scrollHeight - stream.scrollTop - stream.clientHeight < 120;
}

function scrollToBottom(force) {
  const stream = $("stream");
  if (force || isNearBottom()) {
    stream.scrollTop = stream.scrollHeight;
    $("jump").hidden = true;
  }
}

function renderEntries() {
  if (!session) return;
  const stick = isNearBottom();
  const container = $("entries");
  for (const entry of session.entries) {
    if (!entry.node) {
      entry.node = element("div", `entry ${entry.kind}-entry`);
      container.appendChild(entry.node);
      entry.dirty = true;
    }
    if (!entry.dirty) continue;
    entry.dirty = false;
    if (entry.kind === "tool") {
      renderToolCall(entry);
    } else if (entry.kind === "thought") {
      renderThought(entry);
    } else {
      const body = element("div", "markdown");
      body.appendChild(renderMarkdown(entry.text));
      entry.node.replaceChildren(body);
      for (const attachment of entry.attachments) {
        const chip = element("span", "attachment");
        chip.append(icon(attachment.icon), element("span", "", attachment.label));
        entry.node.appendChild(chip);
      }
    }
  }
  // Requests for tool calls that arrived before the tool call itself.
  for (const [id, request] of session.requests) {
    if (request.standalone && attachRequest(id, request)) request.standalone = false;
  }
  if (stick) {
    scrollToBottom(true);
  } else if (session.unseen) {
    $("jump").hidden = false;
  }
  session.unseen = false;
}

function renderThought(entry) {
  let details = entry.node.querySelector("details");
  if (!details) {
    details = element("details", "thought");
    const summary = element("summary", "disclosure");
    summary.append(icon("chevron-right", "chevron"), element("span", "", "思考过程"));
    details.appendChild(summary);
    details.appendChild(element("div", "markdown"));
    entry.node.replaceChildren(details);
  }
  details.querySelector(".markdown").replaceChildren(renderMarkdown(entry.text));
}

function renderToolCall(entry) {
  const data = entry.data;
  let details = entry.node.querySelector(":scope > details");
  const wasOpen = details?.open;
  if (!details) {
    details = element("details", "tool");
    entry.node.replaceChildren(details);
  }
  const status = data.status || "pending";
  const head = element("summary", "tool-head");
  head.appendChild(stateMark(status, { pending: "等待中", in_progress: "执行中", completed: "已完成", failed: "失败" }[status]));
  const title = element("span", "tool-title");
  title.appendChild(icon(TOOL_ICONS[data.kind] || "tool", "tool-kind"));
  title.appendChild(element("span", "", data.title || "工具调用"));
  head.appendChild(title);
  head.appendChild(element("span"));
  head.appendChild(icon("chevron-down", "chevron"));

  const body = element("div", "tool-body");
  if (Array.isArray(data.locations) && data.locations.length) {
    const locations = element("div", "locations");
    for (const location of data.locations) {
      locations.appendChild(pathElement("location", location.path + (location.line ? `:${location.line}` : "")));
    }
    body.appendChild(locations);
  }
  let hasDiff = false;
  for (const item of data.content || []) {
    if (item.type === "diff") {
      hasDiff = true;
      body.appendChild(renderDiff(item));
    } else if (item.type === "content" && item.content) {
      if (item.content.type === "text") {
        const output = element("div", "markdown tool-output");
        output.appendChild(renderMarkdown(item.content.text || ""));
        body.appendChild(output);
      } else {
        body.appendChild(element("div", "attachment", item.content.type));
      }
    } else if (item.type === "terminal") {
      body.appendChild(element("p", "notice", "命令输出由 agent 在服务器上执行，这里不显示终端。"));
    }
  }

  const requestNode = details.querySelector(":scope > .request");
  details.replaceChildren(head);
  if (body.childElementCount) details.appendChild(body);
  if (requestNode) details.appendChild(requestNode);
  details.classList.toggle("awaiting", Boolean(requestNode));
  // Edits are what people review on the go, so a diff opens on its own once.
  details.open = Boolean(wasOpen || requestNode || (hasDiff && !entry.opened));
  if (hasDiff) entry.opened = true;
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
      ...oldLines.map((line, index) => ({ type: "removed", line, number: index + 1 })),
      ...newLines.map((line, index) => ({ type: "added", line, number: index + 1 })),
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
      result.push({ type: "same", line: newLines[column], number: column + 1 });
      row++;
      column++;
    } else if (lengths[(row + 1) * width + column] >= lengths[row * width + column + 1]) {
      result.push({ type: "removed", line: oldLines[row], number: row + 1 });
      row++;
    } else {
      result.push({ type: "added", line: newLines[column], number: column + 1 });
      column++;
    }
  }
  while (row < rows) result.push({ type: "removed", line: oldLines[row], number: ++row });
  while (column < columns) result.push({ type: "added", line: newLines[column], number: ++column });
  return result;
}

function splitLines(text) {
  if (!text) return [];
  const lines = text.split("\n");
  if (lines[lines.length - 1] === "") lines.pop();
  return lines;
}

function renderDiff(item) {
  const container = element("div", "diff");
  const changes = diffLines(splitLines(item.oldText), splitLines(item.newText));
  const added = changes.filter((change) => change.type === "added").length;
  const removed = changes.filter((change) => change.type === "removed").length;

  const head = element("div", "diff-head");
  head.appendChild(element("span", "diff-name", basename(item.path)));
  head.appendChild(pathElement("diff-dir", dirname(item.path)));
  const stats = element("span", "diff-stats");
  stats.append(element("span", "plus", `+${added}`), document.createTextNode(" "), element("span", "minus", `−${removed}`));
  head.appendChild(stats);
  container.appendChild(head);

  const lines = element("div", "diff-lines");
  const visible = changes.map(() => false);
  changes.forEach((change, index) => {
    if (change.type === "same") return;
    for (let offset = -DIFF_CONTEXT; offset <= DIFF_CONTEXT; offset++) {
      if (changes[index + offset]) visible[index + offset] = true;
    }
  });
  let skipped = 0;
  const flushSkipped = () => {
    if (skipped > 0) lines.appendChild(element("div", "diff-skip", `${skipped} 行未改动`));
    skipped = 0;
  };
  changes.forEach((change, index) => {
    if (!visible[index]) {
      skipped++;
      return;
    }
    flushSkipped();
    const line = element("div", `diff-line ${change.type}`);
    line.append(
      element("span", "number", String(change.number)),
      element("span", "mark"),
      element("span", "text", change.line || " "),
    );
    lines.appendChild(line);
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
      fragment.appendChild(renderCodeBlock(code.join("\n")));
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

function renderCodeBlock(code) {
  const block = element("div", "code-block");
  const pre = element("pre");
  pre.appendChild(element("code", "", code));
  const copy = element("button", "icon-button copy");
  copy.type = "button";
  copy.setAttribute("aria-label", "复制代码");
  copy.appendChild(icon("copy"));
  copy.addEventListener("click", async () => {
    if (await copyText(code)) {
      copy.classList.add("copied");
      copy.replaceChildren(icon("check"));
      setTimeout(() => {
        copy.classList.remove("copied");
        copy.replaceChildren(icon("copy"));
      }, 1500);
    }
  });
  block.append(pre, copy);
  return block;
}

async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    // The clipboard API needs HTTPS; plain HTTP inside a VPN falls back here.
    const area = element("textarea");
    area.value = text;
    area.setAttribute("readonly", "");
    area.className = "visually-hidden";
    document.body.appendChild(area);
    area.select();
    const copied = document.execCommand("copy");
    area.remove();
    return copied;
  }
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
  requestAnimationFrame(updateAttention);
  const entries = session.plan || [];
  const plan = $("plan");
  plan.hidden = entries.length === 0;
  const done = entries.filter((entry) => entry.status === "completed").length;
  $("plan-progress").textContent = `${done}/${entries.length}`;
  const current = entries.find((entry) => entry.status === "in_progress");
  const currentNode = $("plan-current");
  currentNode.replaceChildren();
  if (current) appendInline(currentNode, current.content || "");
  else if (done === entries.length) currentNode.textContent = "全部完成";
  const list = $("plan-list");
  list.replaceChildren();
  for (const entry of entries) {
    const item = element("li", entry.status || "pending");
    item.appendChild(stateMark(entry.status || "pending"));
    const text = element("span", "plan-text");
    appendInline(text, entry.content || "");
    item.appendChild(text);
    list.appendChild(item);
  }
  updateDockHeight();
}

function renderControls() {
  if (!session) return;
  const container = $("controls");
  container.replaceChildren();
  // Agents that offer config options often repeat their modes there (Codex's
  // "mode" option, for instance). Like Zed, prefer the config options and
  // ignore the older modes whenever an agent provides both.
  const modes = session.configOptions ? null : session.modes;
  if (modes && Array.isArray(modes.availableModes) && modes.availableModes.length > 1) {
    const choices = modes.availableModes.map((mode) => ({ value: mode.id, name: mode.name || mode.id }));
    container.appendChild(
      selectControl("模式", choices, modes.currentModeId, async (value) => {
        await request("session/set_mode", { sessionId: session.id, modeId: value });
        session.modes.currentModeId = value;
      }),
    );
  }
  for (const option of session.configOptions || []) {
    const control = configControl(option);
    if (control) container.appendChild(control);
  }
}

function selectControl(label, choices, current, onChange) {
  const wrapper = element("label", "select");
  wrapper.title = label;
  const select = element("select");
  select.setAttribute("aria-label", label);
  for (const choice of choices) {
    const option = element("option", "", choice.name);
    option.value = choice.value;
    option.selected = choice.value === current;
    select.appendChild(option);
  }
  select.addEventListener("change", async () => {
    select.disabled = true;
    try {
      await onChange(select.value);
    } catch (error) {
      appendNotice(`修改${label}失败：${error.message}`, true);
    }
    renderControls();
  });
  wrapper.append(select, icon("chevron-down"));
  return wrapper;
}

function configControl(option) {
  const label = option.name || option.id;
  const apply = async (value) => {
    const result = await request("session/set_config_option", {
      sessionId: session.id,
      configId: option.id,
      ...value,
    });
    if (result && result.configOptions) session.configOptions = result.configOptions;
  };
  if (option.type === "boolean") {
    const wrapper = element("label", "toggle");
    const checkbox = element("input");
    checkbox.type = "checkbox";
    checkbox.checked = option.currentValue === true;
    checkbox.addEventListener("change", async () => {
      checkbox.disabled = true;
      try {
        await apply({ type: "boolean", value: checkbox.checked });
      } catch (error) {
        appendNotice(`修改${label}失败：${error.message}`, true);
      }
      renderControls();
    });
    wrapper.append(checkbox, document.createTextNode(label));
    return wrapper;
  }
  if (option.type === "select") {
    const choices = [];
    for (const item of option.options || []) {
      if (Array.isArray(item.options)) choices.push(...item.options);
      else choices.push(item);
    }
    return selectControl(
      label,
      choices.map((choice) => ({ value: choice.value, name: choice.name || choice.value })),
      option.currentValue,
      (value) => apply({ value }),
    );
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
  let node;
  if (message.method === "session/request_permission") {
    node = renderPermission(message);
  } else if (message.method === "elicitation/create") {
    node = renderElicitation(message);
  } else {
    send({ id: message.id, error: { code: -32601, message: "Method not found" } });
    return;
  }
  const request = { node, toolCallId: params.toolCall?.toolCallId, standalone: true };
  session.requests.set(message.id, request);
  if (attachRequest(message.id, request)) request.standalone = false;
  else $("entries").appendChild(node);
  updateStatus();
  scrollToBottom(true);
}

// Shows a permission request inside the tool call it is about, as Zed does.
function attachRequest(id, request) {
  const tool = request.toolCallId && session.toolCalls.get(request.toolCallId);
  const details = tool?.node?.querySelector(":scope > details");
  if (!details) return false;
  details.appendChild(request.node);
  details.classList.add("awaiting");
  details.open = true;
  return true;
}

function removeRequest(id) {
  if (!session) return;
  const request = session.requests.get(id);
  if (!request) return;
  const details = request.node.parentElement;
  request.node.remove();
  details?.classList.remove("awaiting");
  session.requests.delete(id);
  updateStatus();
}

function answer(id, result) {
  try {
    send({ id, result });
  } catch (error) {
    appendNotice(`回答失败：${error.message}`, true);
    return;
  }
  removeRequest(id);
}

function updateAttention() {
  const bar = $("attention");
  if (!session || session.requests.size === 0) {
    bar.hidden = true;
    updateDockHeight();
    return;
  }
  const first = session.requests.values().next().value.node;
  const stream = $("stream").getBoundingClientRect();
  const box = first.getBoundingClientRect();
  // A card with only its edge showing above the dock still needs the prompt.
  const inView = box.bottom > stream.top + 48 && box.top < stream.bottom - 48;
  bar.hidden = inView;
  $("attention-text").textContent =
    session.requests.size === 1 ? "有 1 个请求等待你处理" : `有 ${session.requests.size} 个请求等待你处理`;
  updateDockHeight();
}

$("attention-go").addEventListener("click", () => {
  const first = session?.requests.values().next().value;
  first?.node.scrollIntoView({ behavior: "smooth", block: "center" });
});

function renderPermission(message) {
  const params = message.params;
  const card = element("div", "request");
  const head = element("div", "request-head");
  head.append(icon("shield"), element("span", "", params.toolCall?.title || "agent 请求执行一个操作"));
  card.appendChild(head);
  const options = element("div", "request-options");
  for (const option of params.options || []) {
    const kind = option.kind || "";
    const style = kind.startsWith("allow") ? "allow" : kind.startsWith("reject") ? "reject" : "";
    const button = element("button", `button small ${style}`);
    button.type = "button";
    button.appendChild(icon(style === "reject" ? "x" : "check"));
    button.appendChild(element("span", "", option.name || option.optionId));
    button.addEventListener("click", () => {
      answer(message.id, { outcome: { outcome: "selected", optionId: option.optionId } });
    });
    options.appendChild(button);
  }
  card.appendChild(options);
  return card;
}

function renderElicitation(message) {
  const params = message.params;
  const card = element("div", "request");
  const head = element("div", "request-head");
  head.append(icon("question"), element("span", "", params.message || "agent 需要你提供信息"));
  card.appendChild(head);

  if (params.mode === "url") {
    const options = element("div", "request-options");
    const open = element("button", "button small primary", "打开链接");
    open.type = "button";
    open.addEventListener("click", () => {
      window.open(params.url, "_blank", "noopener,noreferrer");
      answer(message.id, { action: "accept" });
    });
    const decline = element("button", "button small reject", "拒绝");
    decline.type = "button";
    decline.addEventListener("click", () => answer(message.id, { action: "decline" }));
    options.append(open, decline);
    card.appendChild(options);
    return card;
  }

  const schema = params.requestedSchema || {};
  const required = new Set(schema.required || []);
  const form = element("form", "request-form");
  const fields = [];
  for (const [name, property] of Object.entries(schema.properties || {})) {
    let input;
    let field;
    const title = property.title || name;
    const choices =
      property.enum ||
      property.oneOf?.map((choice) => choice.const) ||
      property.anyOf?.map((choice) => choice.const);
    if (property.type === "boolean") {
      field = element("label", "check");
      input = element("input");
      input.type = "checkbox";
      input.checked = property.default === true;
      field.append(input, document.createTextNode(title));
    } else {
      field = element("label", "field");
      field.appendChild(element("span", "field-label", required.has(name) ? `${title}（必填）` : title));
      if (choices) {
        input = element("select");
        const titles = property.enumNames || property.oneOf || property.anyOf;
        choices.forEach((choice, index) => {
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
      field.appendChild(input);
      if (property.description) field.appendChild(element("span", "field-hint", property.description));
    }
    form.appendChild(field);
    fields.push({ name, property, input });
  }
  const buttons = element("div", "request-options");
  const submit = element("button", "button small primary", "提交");
  submit.type = "submit";
  const decline = element("button", "button small reject", "拒绝");
  decline.type = "button";
  decline.addEventListener("click", () => answer(message.id, { action: "decline" }));
  const cancel = element("button", "button small ghost", "取消");
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
  return card;
}

// ---------------------------------------------------------------------------
// Composer

const textarea = $("message");

function updateComposer() {
  if (!session) return;
  const dead = session.dead;
  $("composer").hidden = dead;
  $("dock-note").hidden = !dead;
  $("dock-note").textContent = dead ? "agent 进程已经退出，这个会话只能查看。" : "";
  const hasText = textarea.value.trim().length > 0;
  // While a turn runs, the button stops it; typing turns it back into send,
  // which interrupts the turn after confirmation.
  const showStop = session.running && !hasText;
  $("stop").hidden = !showStop;
  $("send").hidden = showStop;
  $("send").disabled = !hasText || session.loading || !connection.connected;
  $("send-hint").textContent = COARSE.matches ? "" : `${navigator.platform.startsWith("Mac") ? "⌘" : "Ctrl"} ↵ 发送`;
  updateDockHeight();
}

function updateDockHeight() {
  const dock = $("dock");
  if (!dock.hidden) $("thread").style.setProperty("--dock-height", `${dock.offsetHeight + 8}px`);
}

textarea.addEventListener("input", () => {
  textarea.style.height = "auto";
  textarea.style.height = `${Math.min(textarea.scrollHeight, window.innerHeight * 0.4)}px`;
  updateComposer();
});

textarea.addEventListener("keydown", (event) => {
  if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
    event.preventDefault();
    $("composer").requestSubmit();
  }
});

$("composer").addEventListener("submit", async (event) => {
  event.preventDefault();
  if (!session || session.dead) return;
  const text = textarea.value.trim();
  if (!text) return;
  if (session.running && !confirm("agent 正在执行。发送新消息会中断当前这一轮，继续吗？")) return;

  const current = session;
  textarea.value = "";
  textarea.style.height = "auto";
  applyUpdate({ sessionUpdate: "user_message_chunk", content: { type: "text", text } });
  // Keeps the next message the user sends out of this one.
  session.entries[session.entries.length - 1].messageId = `local-${Date.now()}`;
  renderEntries();
  setRunning(true);
  scrollToBottom(true);
  try {
    const result = await request("session/prompt", {
      sessionId: current.id,
      prompt: [{ type: "text", text }],
    });
    if (session === current) reportStop(result.stopReason);
  } catch (error) {
    if (session === current) appendNotice(`发送失败：${error.message}`, true);
  }
  if (session === current) setRunning(false);
});

$("stop").addEventListener("click", () => {
  if (!session) return;
  try {
    send({ method: "session/cancel", params: { sessionId: session.id } });
    $("stop").disabled = true;
    setTimeout(() => ($("stop").disabled = false), 1500);
  } catch (error) {
    appendNotice(`停止失败：${error.message}`, true);
  }
});

$("stream").addEventListener(
  "scroll",
  () => {
    if (isNearBottom()) $("jump").hidden = true;
    updateAttention();
  },
  { passive: true },
);

$("jump").addEventListener("click", () => {
  $("stream").scrollTo({ top: $("stream").scrollHeight, behavior: "smooth" });
  $("jump").hidden = true;
});

window.addEventListener("resize", updateDockHeight);
$("plan").addEventListener("toggle", () => requestAnimationFrame(updateAttention));
WIDE.addEventListener("change", highlightSelection);

// ---------------------------------------------------------------------------
// Start

if ("serviceWorker" in navigator) {
  navigator.serviceWorker.register("/sw.js").catch(() => {});
}

(async () => {
  $("app").dataset.view = currentRoute() ? "thread" : "list";
  showThreadEmpty();
  const health = await fetchHealth();
  if (health && !health.authenticated) {
    showLogin();
  } else {
    connect();
  }
})();
