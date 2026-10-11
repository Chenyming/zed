---
title: "Zed 远程开发通信与 ACP 代理机制"
description: "host Zed 与 zed-remote-server 的通信链路、外部 ACP 代理的启动与生命周期管理，以及断连后的已知问题。"
---

# Zed 远程开发通信与 ACP 代理机制

本文基于 commit `36dd5531680f1bbd57002dbcb42d76ac4d87a68f`（`crates/zed`
版本 `1.17.0`）整理，是源码阅读笔记，不是上游正式规范。行号会随代码演进
漂移，引用时请以当前源码为准。

配套阅读：[Zed 架构与技术栈速查](./architecture-overview.zh-CN.md)、
[远程开发](../remote-development.md)。

## 总体拓扑 {#topology}

host 只维护一个 `ssh` 子进程的 stdio。远端是「无状态 proxy + 常驻
headless server」两层进程。

```text
  Host Zed（本地）                     远程主机
┌───────────────────────┐        ┌──────────────────────────────┐
│ Project / Workspace   │        │ proxy 进程                   │
│   └ AnyProtoClient    │        │   execute_proxy()            │
│       └ ChannelClient │  ssh   │     ↕ stdin/stdout/stderr    │
│           └ stdio 泵 ─┼────────┼→    ↕ 三个 unix socket       │
│               ssh 子进程       │ run 进程（常驻 headless）    │
└───────────────────────┘        │   GPUI headless app          │
                                 │   └ HeadlessProject          │
                                 │       Worktree/Buffer/Lsp/…  │
                                 └──────────────────────────────┘
```

`crates/remote_server` 是单一二进制，靠子命令分角色
（`server.rs` 的 `enum Commands`）：

| 子命令    | 角色                                       |
| --------- | ------------------------------------------ |
| `run`     | 常驻 headless 服务，承载全部项目状态       |
| `proxy`   | 无状态字节转发层，桥接 ssh stdio 与 socket |
| `version` | 打印版本                                   |

## 线路协议 {#wire-protocol}

最底层帧格式定义在 `crates/remote/src/protocol.rs`：

```text
[u32 小端长度][prost 编码的 zed.messages.Envelope]
```

- `write_message` / `read_message`：两端收发，解码为 `Envelope`。
- `read_message_raw` / `write_size_prefixed_buffer`：proxy 专用，只搬
  字节，不解码 protobuf。

`Envelope`（`crates/proto/proto/zed.proto`）携带 `id`、`responding_to`、
`ack_id`（field 266）和 `oneof payload`。

> **Note:** `Envelope` 里没有 connection id 字段。同一条字节流上所有 RPC
> 靠 `id` 与 `responding_to` 配对来多路复用。

## 三条管道的分工 {#pipes}

host 侧的泵是 `transport.rs` 的
`handle_rpc_messages_over_child_process_stdio`，被 ssh、wsl、docker 三种
transport 共用。

| 管道   | 方向          | 用途                                |
| ------ | ------------- | ----------------------------------- |
| stdin  | host → remote | 从 `outgoing_rx` 取 Envelope 写入   |
| stdout | remote → host | 解出 Envelope，喂心跳活动信号并投递 |
| stderr | remote → host | 逐行 JSON 日志，不是 RPC            |

stderr 上跑的是 `json_log.rs` 的 `LogRecord`，解析后注入 host 本地 `log`
系统；解析失败则原样打印 `(remote) ...`。远端 server 侧双写：本地滚动日志
文件（上限 1 MB）与 stderr socket 各一份。

## 连接建立 {#connection-setup}

```text
remote::connect                       remote_client.rs
 └ ConnectionPool::connect            按连接选项去重复用
    └ SshRemoteConnection::new        ssh.rs
       ├ 认证：复用已有 ControlMaster，或起 ssh -N + askpass
       ├ 探测远端 platform / shell / path_style
       └ ensure_server_binary
RemoteClient::new
 ├ ChannelClient::new
 ├ remote_connection.start_proxy(...)
 ├ wait_for_remote_started()          等对端 RemoteStarted 握手
 ├ monitor(io_task)                   盯 ssh 进程退出码
 └ heartbeat(...)
```

远端实际执行的命令形如：

```sh
ssh -o ControlPath=<sock> <dest> -T \
  "cd; env RUST_LOG=... <remote_binary> proxy --identifier <id> [--reconnect]"
```

关键超时与重试常量（`remote_client.rs`）：

| 常量                         | 值                          |
| ---------------------------- | --------------------------- |
| `HEARTBEAT_INTERVAL`         | 5s                          |
| `HEARTBEAT_TIMEOUT`          | 5s                          |
| `MAX_MISSED_HEARTBEATS`      | 5                           |
| `MAX_RECONNECT_ATTEMPTS`     | 3                           |
| `INITIAL_CONNECTION_TIMEOUT` | 5s（debug）/ 60s（release） |
| `IDLE_TIMEOUT`（server 侧）  | 10 分钟                     |

## 远程二进制分发 {#server-binary}

`SshRemoteConnection::ensure_server_binary`：

1. 命名为 `zed-remote-server-{channel}-{version}`，装在远端
   `~/.local/share/zed/remote_server/`。
2. 先跑 `<dst> version` 判断是否已存在。只查存在性，不做版本比对。
3. 缺失时优先让远端自己 `curl` 下载，`curl` 不存在时回退 `wget`。
4. 配置了 `upload_binary_over_ssh` 或下载失败时，本地下载再经 SFTP 上传，
   SFTP 不可用时回退 `scp`。

## 服务端消息处理 {#headless-project}

`HeadlessProject`（`crates/remote_server/src/headless_project.rs`）不包装
`Project`，而是直接组合各 store，每个都调用
`.shared(REMOTE_SERVER_PROJECT_ID, session, cx)`：`WorktreeStore`、
`BufferStore`、`LspStore`、`GitStore`、`DapStore`、`TaskStore`、
`SettingsObserver`、`AgentServerStore` 等。

路由分两层：

1. `session.subscribe_to_entity(...)` 注册 11 个实体，按 envelope 的
   entity id 分发到 store 自己的 handler。
2. `HeadlessProject` 注册少量全局 handler：`Ping`、
   `ListRemoteDirectory`、`GetPathMetadata`、`ShutdownRemoteServer`、
   `OpenBufferByPath`、`FindSearchCandidates`、`DownloadFileByPath`、
   `GetCrashFiles` 等。

反向推送通过订阅事件完成，例如 buffer 变更发 `proto::UpdateBuffer`，LSP
状态变化发 `proto::UpdateLanguageServer`。

## 心跳、重连与消息重放 {#reliability}

**心跳**：每 5s 一轮。期间 stdout 或 stderr 有任何流量即判活，否则发
`proto::Ping`。连续 5 次失败触发重连。server 端不主动保活，只应答 Ping。

**重连**（上限 3 次）：

1. kill 旧 ssh 进程。
2. 重新 `start_proxy(identifier, reconnect = true)`。远端 proxy 读
   `server.pid` 校验旧 server 存活，则重新接到同一 server 的三个 socket，
   项目状态原封不动；若 server 已死则以退出码 **90** 退出，host 转入
   `ServerNotRunning` 终态。
3. `ChannelClient::reconnect` 换掉底层 channel。
4. `resync` 发 `FlushBufferedMessages` 并重放缓冲。

**at-least-once 重放**：`ChannelClient` 维护 `buffer: VecDeque<Envelope>`
和 `max_received`。每条出站消息捎带 `ack_id = max_received`，对端据此修剪
自己的 buffer。重连后双方互发 `FlushBufferedMessages` 完成双向重放，去重
依赖上层 handler 幂等。

**服务端存活策略**：accept 循环在连接断开后 break 内层循环，回到外层继续
accept；`HeadlessProject` 被 `mem::forget` 保证永不 drop。兜底是 10 分钟
`IDLE_TIMEOUT`，超时无新连接则自行退出。

## 连接状态机 {#state-machine}

内部 `State` 对外投影为 `ConnectionState`：

```text
        Connecting
            │ RemoteStarted 握手 + ping 成功
            ▼
    ┌── Connected ──────────────┐
    │       │ 心跳超时           │ ssh 进程异常退出
    │       ▼                   ▼
    │  HeartbeatMissed ──→ Reconnecting ──→ Connected
    │       │ missed >= 5       │ 任一步失败
    │       │                   ▼
    │       │             ReconnectFailed ──┐
    │       │                   │ 自动重试  │
    │       │                   └───────────┘
    │       │                   │ 超过 3 次
    │       │                   ▼
    │       │            ReconnectExhausted ──→ Disconnected
    │       ▼
    └─→ ServerNotRunning（退出码 90）──────→ Disconnected
```

## ACP 代理的启动 {#acp-launch}

外部 ACP 代理的职责切分是：**远端 server 只负责解析出命令，host 负责执行
命令**。

协议面只有 4 条消息（`zed.proto`）：请求 `GetAgentServerCommand` →
`AgentServerCommand`，外加三条单向通知 `ExternalAgentsUpdated`、
`ExternalAgentLoadingStatusUpdated`、
`NewExternalAgentVersionAvailable`。

`AgentServerStoreState`（`crates/project/src/agent_server_store.rs`）三态：

| 形态     | 位置        | 说明                                         |
| -------- | ----------- | -------------------------------------------- |
| `Local`  | 远端 server | 持有远端 `node_runtime`、`fs`、`http_client` |
| `Remote` | host        | 只存 `project_id` 和 `upstream_client`       |
| `Collab` | —           | 外部代理暂不支持共享项目                     |

启动流程：

1. **发现**：远端 `shared()` 中延迟 1 秒发送 `ExternalAgentsUpdated`。
   延迟是为了规避「消息早于下游 handler 注册到达」的竞态。host 收到后把
   每个名字包成 `RemoteExternalAgentServer`。
2. **请求命令**：host 发 `GetAgentServerCommand { project_id, name,
root_dir }`。
3. **远端解析**：远端调用 `LocalRegistryArchiveAgent` 等实现的
   `get_command`。这一步会 `fs.create_dir`、按 `cfg!(target_os)` 和
   `cfg!(target_arch)` 拼 platform key、用 `http_client` 下载解包。这些
   `cfg!` 编译进 `remote_server` 二进制，所以平台判定天然是远端机器的，
   agent 二进制也装在远端。安装进度经 `watch::channel` 转成
   `ExternalAgentLoadingStatusUpdated` 回传 host。
4. **host 包装并 spawn**：返回的 path 是远端路径，host 通过
   `RemoteClient::build_command` 包成 ssh 命令，再本地
   `Child::spawn(..., Stdio::piped() × 3)`。

`build_command_posix` 产出：

```sh
ssh <既有选项> -o LogLevel=ERROR -T <dest> \
  "cd <quoted_dir> && exec env K=V ... <path> <args>"
```

`Interactive::No` 对应 `-T`（禁 TTY，因为要接管道）；`exec env` 避免多留
一层 shell；`cd` 用 `try_quote` 做注入防护。远程场景的工作目录靠 exec 串
里的 `cd` 完成，`current_dir` 只在本地项目时设置。

## ACP 通道的性质 {#acp-channel}

「独立」需要分两层看：

**进程与数据流层面：完全独立。** 独立的 ssh 子进程、独立的三条管道、独立
的协议（newline-delimited JSON-RPC），与 RPC 主链路的 Envelope 帧协议零
交集。每个 agent 连接一个这样的进程。故障双向隔离。

**底层 TCP 连接：POSIX 上不独立。** `ssh_command_options` 在非 Windows
下强制追加 `-o ControlMaster=no -o ControlPath=<master socket>`，因此 ACP
通道复用同一个 TCP 连接和同一个已认证的 SSH 会话，只是新开一个 channel。

| 影响                | 说明                                       |
| ------------------- | ------------------------------------------ |
| 不二次认证          | 不会再弹密码，启动 agent 几乎零延迟        |
| 无额外 TCP 握手     | 复用 master 连接                           |
| 命运共享            | master 连接一断，所有 ACP channel 同时失效 |
| 受 MaxSessions 约束 | 同一 TCP 上的 channel 数有 sshd 上限       |

> **Note:** Windows 分支不走 ControlMaster，每条 ACP 通道是真正独立的 TCP
> 连接和独立认证。

## ACP 生命周期管理 {#acp-lifecycle}

`AcpConnection` 持有 `child: Option<Child>` 和三个 task（`_io_task`、
`_wait_task`、`_stderr_task`）。`Drop` 时调用 `child.kill()`。

上层 `AgentConnectionStore` 按 `Agent` key 缓存
`entries: HashMap<Agent, Entity<AgentConnectionEntry>>`。

**断连检测只有一条路径：子进程退出。** `child.status()` 解析后经
`wait_task` 调 `emit_load_error_to_all_sessions`，把
`LoadError::Exited { status, .. }` 广播给该连接下所有 session 的
`AcpThread`，UI 转入 `ServerState::LoadError`。

> **Warn:** ACP 层没有心跳。检测完全依赖 ssh 进程真的退出。若 TCP 黑洞而
> ssh 未退出，host 不会有任何感知。

与主 RPC 链路的对比：

| 能力     | 主 RPC 链路                   | ACP 通道   |
| -------- | ----------------------------- | ---------- |
| 心跳     | 5s Ping × 5                   | 无         |
| 自动重连 | `reconnect()` × 3             | 无         |
| 远端状态 | server 常驻，`mem::forget`    | 随通道死亡 |
| 消息重放 | `ack_id` + `FlushBuffered...` | 无缓冲     |

根因在「远端状态」一行：headless server 独立于连接生命周期，所以能原地等
下一根线；而 agent 进程是 ssh exec 出来的前台子进程，没有 pid 文件、没有
socket、没有可重新附着的锚点，**没有东西可以重连回去**。

恢复只能从头重建，入口是 `ConversationView::reset()`，触发点仅两类：

1. `handle_agent_servers_updated`：`AgentServersUpdated` 到来且当前处于
   `LoadError` 时重试。
2. 认证成功之后。

外加开发者工具：`acp_tools` 面板的 RotateCw「Restart Connection」按钮，
这是生产代码里 `restart_connection` 的唯一调用点。

`reset()` 会保留原 session id 交给 ACP 的 session 恢复流程，
`conversation_view.rs` 有测试断言
`"reset() must resume the original session id, not call new_session()"`。

> **Warn:** `request_connection` 开头会直接返回已缓存的 entry，而
> `entries.remove` 只发生在手动 `restart_connection`、连接建立时失败、有
> 新版本可用、agent 从远端列表消失四种情况。「已连接成功、之后子进程死
> 掉」不在其中，因此 entry 会以 `Connected` 状态保留一个已死的 child。

## 已知问题：Codex active writer 冲突 {#known-issue-active-writer}

Codex 对每个可写 thread 持有操作系统文件锁。另一个 Codex 进程恢复同一
thread 时，可能返回：

```text
thread <uuid> already has an active writer
```

SSH 网络分区留下的远端 `codex-acp` / `codex app-server` 孤儿进程是可能原因
之一，但相同错误也可能由 Codex CLI、Desktop、Remote Control、编辑器扩展或
另一个 Zed 会话引起。不要直接删除仍被持有的 lock 文件，也不要用无差别的
`pkill` 绕过单写者保护。

错误归属、准确定位持锁 PID、安全恢复步骤、SSH 缓解配置及长期修复建议见独立
文档：[远程开发中的 Codex active writer 冲突](./codex-active-writer-conflict.zh-CN.md)。

## 验证状态与待确认项 {#verification-status}

本文结论来自源码阅读与交叉核对。以下几点属推断或未展开验证，使用时请注意：

- `render_load_error` 只渲染错误提示和复制按钮，不包含显式重试入口；
  `handle_agent_servers_updated` 可以在 `LoadError` 状态触发 `reset()`，但仍会经过
  `AgentConnectionStore` 的连接缓存。
- 测试 `test_reset_preserves_session_id_after_load_error` 断言 `reset()` 会恢复原
  session id，而不是新建 session。
- 各 store 的 handler 行号区间未逐条核对，仅核对了注册点存在性。
- Codex writer lock 的实现和恢复限制另见
  [远程开发中的 Codex active writer 冲突](./codex-active-writer-conflict.zh-CN.md)。
