# acp-mux 实现方案:Zed 远程 Agent 的 attach/detach

> 基于对源码的只读调研(分支 `ymzed`,2026-10-17)。调研过程见 `agent_detach_plan.md`,本文是最终的实现方案。
> 文中的行号对应当前分支,实现前请重新核对。

## 1. 需求

### 1.1 场景

- Linux 上运行 Zed 的 remote_server,作为 server;macOS 上运行 Zed 桌面客户端,作为 client,通过 SSH 远程项目连接。
- client 在 agent 线程里发起任务,之后可以随时断开(detach):关掉笔记本、断网、退出 Zed 都算。**server 上的 agent 继续执行。**
- client 可以随时重新连接(attach),看到**最新的完整状态**:断开期间产生的消息、工具调用、diff,以及挂起的授权请求;如果 agent 还在执行,要正确显示"生成中",可以停止,结束时有通知。
- 行为和 tmux 一样:server 端的会话独立于 client 的连接;多个 client 先后 attach 同一个会话时,后来的挤掉先来的。

### 1.2 范围

| 包括 | 不包括(后续再考虑) |
|---|---|
| 外部 ACP agent(Claude Code、Codex、Gemini CLI,以及注册表和自定义 agent) | Zed 内置 agent(NativeAgent),见 `agent_detach_plan.md` 的 R2 |
| SSH 远程项目 | WSL、Docker、Windows 远端 |
| 单用户、单台 Linux 主机 | 多用户共享 |

### 1.3 现状和差距

| 现状 | 依据 | 结果 |
|---|---|---|
| 外部 agent 进程由 Mac 端通过 ssh 启动,stdio 挂在 ssh 子进程上 | `crates/agent_servers/src/acp/transport.rs:35-63`,`crates/remote/src/transport/ssh.rs:503-541`(经过 `<remote_server> exec` 启动) | ssh 一断,agent 就结束 |
| remote_server 每个 workspace 一个进程;新建连接时杀掉旧进程;Zed 退出时发送 `ShutdownRemoteServer`;空闲 10 分钟退出 | `crates/remote_server/src/server.rs:1001-1006, 520-560`,`crates/project/src/project.rs:1638, 2044` | 不能把 agent 托管在 remote_server 进程里 |
| 对外部 agent,Zed 只有在自己发出 `prompt` 时才把线程置为 Running | `crates/acp_thread/src/acp_thread.rs:6441-6467`(`run_turn`) | attach 时如果 agent 还在执行,Zed 会显示为空闲 |

## 2. 总体方案

```
 macOS Zed(SSH 远程项目)
   │ ① 启动 agent 时向 Linux 请求命令(GetAgentServerCommand)
   ▼
 remote_server(workspace-N,随连接创建和销毁)
   │ ② 返回改写后的命令:<remote_server 自身路径> acp-mux attach --agent <name> -- <真实命令...>
   ▼
 Mac 通过 ssh 执行 `<remote_server> exec`,再由它执行上面的命令
   ▼
 acp-mux attach(薄客户端:stdio ⇄ unix socket,随 ssh 连接创建和退出)
   │ ③ 连接 daemon,不存在就拉起
   ▼
 acp-mux daemon(每个用户一个,常驻,类似 tmux server)
   │ ④ 每个 agent 长期持有一个进程;维护会话状态;对 Zed 扮演 agent,对 agent 扮演永远在线的客户端
   ▼
 claude-code-acp / codex-acp …(常驻)
```

要点:

1. **acp-mux 作为 remote_server 二进制的子命令**,跟着已有的 `ZED_COPY_REMOTE_SERVER` 流程一起上传,不需要单独部署。
2. **在服务端改写命令**:所有外部 agent 自动接入 mux。不做"关闭运行中状态"补丁的话,Mac 端不用改。
3. **在 Mac 端打一个小补丁**:解决 attach 后正在执行的轮次显示不对的问题(第 6 节)。没有这个补丁也能用,只是 attach 后看不到"生成中"。

## 3. acp-mux 详细设计

### 3.1 进程和文件

| 项 | 取值 |
|---|---|
| 状态目录 | `paths::remote_server_state_dir()/acp-mux/`(即 `~/.local/share/zed/server_state/acp-mux/`),权限 0700 |
| daemon socket | `<状态目录>/daemon.sock`,权限 0600 |
| pid 文件 / 锁 | `<状态目录>/daemon.pid`,用 `flock` 保证只有一个 daemon |
| 日志 | `logs_dir()/acp-mux.log` |
| 会话持久化(可选,M3) | `<状态目录>/sessions/<agent>/<session_id>.jsonl` |
| 配置 | `~/.config/zed/acp_mux.json`,例如 `{"enabled": true, "agents": ["claude-acp", "codex-acp"], "auto_approve": []}`。不改 Zed 的 settings schema,降低上游合并的冲突 |

**启动 daemon:** `attach` 连接 socket 失败时,用自己的 `current_exe()` 执行 `acp-mux daemon`,经过 `setsid` 和二次 fork 脱离 ssh 会话,stdio 重定向到日志,然后重试连接,带指数退避,最多等 5 秒。daemon 不能依赖自身二进制的路径:上传新版本 remote_server 后,旧文件会被 rename 替换掉。

### 3.2 子命令

```
remote_server acp-mux attach --agent <name> -- <program> [args...]   # 由改写后的命令调用;环境变量和工作目录由 exec 设好
remote_server acp-mux daemon                                          # 由 attach 自动拉起;也可以交给 systemd user service 管理
remote_server acp-mux list                                            # 列出 agent、会话、状态(运行中/空闲/等待授权)、是否有 client attach
remote_server acp-mux kill <session_id|--agent name>                  # 真正结束会话或 agent
remote_server acp-mux restart                                         # 升级 daemon,会中断正在执行的任务
```

### 3.3 attach 和 daemon 之间的协议

- 帧格式:每行一个 JSON。第一行是控制握手,之后透传 ACP 的 JSON-RPC 行。
- 握手:`{"acp_mux": 1, "agent": "<name>", "command": {"program": ..., "args": [...], "env": {...}, "cwd": "..."}}` → `{"ok": true, "daemon_version": "<ZED_PKG_VERSION>", "protocol": 1}`。协议版本不兼容时,daemon 返回错误,attach 把错误写到 stderr 后退出;Zed 会把它显示成 agent 启动失败。
- `env` 里可能有 API key。它只在本机 unix socket 上传输,socket 权限是 0600。

### 3.4 Agent 进程模型

- daemon 按 `(agent 名, 命令哈希)` 维护 agent 进程:**第一次 attach 时启动,之后一直保留**;只有执行 `kill` 或者 agent 自己退出时才结束。
- **只 `initialize` 一次。** daemon 作为客户端,对 agent 宣告的能力是:
  ```json
  {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false, "auth": {"terminal": false}}
  ```
  这样 agent 在 Linux 上直接用自己的工具读写文件和执行命令,有没有 client 连着,行为都一样(代价见第 9 节)。
- 缓存 agent 返回的 `InitializeResponse`。之后每个 attach 上来的 Zed 发来 `initialize` 时,daemon 用**改写过的缓存**回答:
  - `agentCapabilities.loadSession = true`(由 daemon 自己实现,不依赖 agent 是否支持);
  - `agentCapabilities.sessionCapabilities.list = {}`;
  - 删除 `resume`,让 Zed 统一走 load 路径;
  - `_meta["zed.dev/acpMux"] = {"protocol": 1}`,Zed 的补丁据此开启第 6 节的行为。
- `authenticate` 直接透传。如果 agent 需要登录,第一次 attach 时在 Zed 里完成,凭据由 agent 自己保存在 Linux 上。

### 3.5 会话状态(daemon 内部)

```rust
struct MuxSession {
    session_id: String,
    agent: AgentKey,
    cwd: PathBuf,
    title: Option<String>,
    updated_at: SystemTime,
    snapshot: SessionSnapshot,             // 压缩后的当前状态,用于重放
    turn: TurnState,                       // Idle { last_stop_reason } | Running { agent_request_id, started_at }
    pending_agent_requests: Vec<PendingAgentRequest>, // 挂起的 request_permission 和 elicitation
    observer: Option<ClientId>,            // 当前 attach 着的 client,最多一个
}
```

**`SessionSnapshot` 的压缩规则**(重放的不是原始日志,而是当前状态):

| session/update 类型 | 合并方式 |
|---|---|
| `user_message_chunk` / `agent_message_chunk` / `agent_thought_chunk` | 相邻且类型相同(并且 `messageId` 相同或者都为空)的文本块拼接成一块;非文本块单独保留 |
| `tool_call` + `tool_call_update` | 按 `toolCallId` 合并成一条完整的 `tool_call`:update 中出现的字段覆盖原值,`content` 和 `locations` 出现时整体替换 |
| `plan`、`available_commands_update`、`current_mode_update`、`config_option_update`、`session_info_update`、`usage_update` | 只保留最后一次 |
| 其他和未知的类型 | 原样保留(按顺序追加) |

**用户消息:** Zed 发出 `session/prompt` 时,daemon 把 `prompt` 内容以 `user_message_chunk` 的形式写进 snapshot,这样重放时能看到用户说过什么。

### 3.6 消息路由

**Zed → daemon(daemon 扮演 agent):**

| 方法 | 处理 |
|---|---|
| `initialize` | 用缓存回答(第 3.4 节) |
| `session/new` | 转发给 agent;把这个会话登记为 `MuxSession`,observer 设为当前 client |
| `session/list` | 由 daemon 用 `MuxSession` 列表回答(会话 id、cwd、标题、更新时间);支持 `cwd` 过滤 |
| `session/load` | **由 daemon 自己处理**,见第 3.7 节 |
| `session/prompt` | 如果有轮次正在执行(这种情况只有在 Zed 没打补丁时才会出现),先给 agent 发 `session/cancel`,等它结束;然后转发,记下 agent 端的请求 id。轮次结束时,把响应回给发起请求的 client(如果它还连着) |
| `session/cancel` | 透传 |
| `session/set_mode`、`set_config_option` 等 | 透传 |
| `session/close` | **拦截**:只解除 observer,回复成功,不转发给 agent |
| 其他请求 | 透传;agent 和 client 两侧的 JSON-RPC id 由 daemon 做映射 |

**agent → daemon(daemon 扮演客户端):**

| 方法 | 处理 |
|---|---|
| `session/update` | 写进 snapshot;如果有 observer,转发给它 |
| `session/request_permission`、`elicitation/create` | 放进 `pending_agent_requests`;有 observer 就转发(用 daemon 分配的新 id),没有就挂起。如果配置了 `auto_approve`,并且匹配工具类型(例如 `read`),daemon 直接选择 allow 选项 |
| `fs/*`、`terminal/*` | 不应该收到(能力已经关闭);收到就返回 method not found |
| `session/prompt` 的响应 | `turn = Idle { stop_reason }`;回复给发起请求的 client;给 observer 发 `turn_state: idle`(第 6 节) |

**client 断开**(attach 进程退出,或者 socket EOF):这个 client 观察的所有会话的 `observer` 置为空;它发起但还没完成的 `prompt` 继续执行,结果只写进状态;已经转发给它、还没回复的授权请求重新回到挂起状态。

### 3.7 attach(`session/load`)的时序

```
Zed                                    daemon                                agent
 │ session/load(sid)                     │                                     │
 │──────────────────────────────────────>│ 如果已有 observer,把旧 client 的 observer 撤掉(tmux attach -d)
 │                                       │ observer = 当前 client
 │ <── session/update × N(重放 snapshot)│
 │ <── _zed.dev/acpMux/turn_state        │ (只在 turn == Running 时发送;排在重放之后、load 响应之前)
 │      {sessionId, state: "running"}    │
 │ <── load 响应(modes/config 取自缓存)│
 │ <── session/request_permission × M    │ (挂起的授权请求,在 load 响应之后发送)
 │ <── 后续的实时 session/update ────────│<────────────────────────────────────│
```

顺序很重要:

- 重放的 update 要早于 load 响应。Zed 在发 load 之前就已经登记了线程(`crates/agent_servers/src/acp.rs:1275-1288`),所以能接收这些 update。
- `turn_state: running` 要早于授权请求。否则 Zed 进入运行状态时调用的 `cancel_inner` 会把还没有运行轮次时收到的授权请求全部取消(`acp_thread.rs:6450` 调用 `cancel_inner`,而它在没有运行轮次时会取消所有 `permission_requests`,见 `:6655-6661`)。
- 如果 agent 进程已经退出(会话丢失),`session/load` 返回错误 "session not found",Zed 会显示加载失败。

**daemon 重启后**(M3,可选):如果 agent 本身支持 `loadSession`,daemon 先用持久化的 snapshot 回应 Zed,再在后台对 agent 发 `session/load`,恢复后续对话的能力。

### 3.8 生命周期和升级

- daemon 没有空闲超时。只要配置是 `enabled`,就一直运行。
- **升级:** 上传新的 remote_server 后,运行中的 daemon 不受影响。新的 attach 握手时检查协议版本:兼容就继续使用旧 daemon;不兼容就报错,提示用户执行 `acp-mux restart`。restart 会在日志里写明哪些运行中的会话被中断了。
- agent 进程异常退出时,把它名下的会话标记为 `Dead`,`list` 中还能看到它们,`load` 时只读重放(返回 snapshot,但之后发 `prompt` 会报错)。

### 3.9 安全

- 只监听 unix socket,不开 TCP 端口;socket 和目录权限分别是 0600 和 0700;连接时用 `SO_PEERCRED` 检查对端 uid 等于自己的 uid。
- `auto_approve` 默认为空;日志里记录每一次授权决定。

## 4. 代码修改方案

### 4.1 改动一览

| # | 位置 | 改动 | 端 |
|---|---|---|---|
| 1 | **新 crate `crates/acp_mux`**(`[lib] path = "src/acp_mux.rs"`) | daemon、attach、snapshot、路由、控制协议、CLI 入口 | 服务端 |
| 2 | `crates/remote_server/src/server.rs:80-95, 98-125` | `Commands` 增加 `AcpMux(acp_mux::Command)`,`run()` 分派给 `acp_mux::run` | 服务端 |
| 3 | `crates/project/src/agent_server_store.rs:613-705` | 增加一个可选的命令包装钩子,在 `handle_get_agent_server_command` 返回前应用 | 服务端(共享 crate) |
| 4 | `crates/remote_server/src/headless_project.rs:254-263` | 构造 `AgentServerStore` 后,根据 `acp_mux.json` 设置包装钩子 | 服务端 |
| 5 | `crates/agent_servers/src/acp.rs` | 识别 `_meta["zed.dev/acpMux"]`;注册 `_zed.dev/acpMux/turn_state` 通知的处理函数;在 `AcpSession` 中保存外部轮次的 sender | **客户端** |
| 6 | `crates/acp_thread/src/acp_thread.rs` | 新增 `attach_external_turn`,复用 `run_turn` 的完成逻辑 | **客户端** |

改动 1–4 只要更新 Linux 上的 remote_server;5–6 需要重新构建 Mac 上的 Zed。两部分可以独立上线。

### 4.2 `acp_mux` crate 的结构

```
crates/acp_mux/
  Cargo.toml          # 依赖:anyhow, serde, serde_json, smol(或 tokio), futures, log, paths, util, clap, libc
  src/acp_mux.rs      # pub enum Command { Attach{..}, Daemon, List, Kill{..}, Restart }; pub fn run(Command) -> Result<()>
  src/attach.rs       # stdio ⇄ socket 双向拷贝;拉起 daemon;握手
  src/daemon.rs       # socket accept;ClientId 分配;总调度循环
  src/agent.rs        # agent 进程管理;initialize 缓存;JSON-RPC id 映射
  src/session.rs      # MuxSession、TurnState、pending 请求、observer
  src/snapshot.rs     # 第 3.5 节的压缩规则(重点写单元测试)
  src/router.rs       # 第 3.6 节的路由表
  src/control.rs      # 握手和控制消息类型、协议版本
```

- **JSON 处理:** 用 `serde_json::Value` 透传。只解析需要的字段(`method`、`id`、`params.sessionId`、`params.update.sessionUpdate`、`toolCallId`、`messageId`),避免 ACP schema 升级后反序列化失败。
- **运行时:** daemon 是普通的异步程序,不需要 GPUI。用 `smol` 就够了(仓库里已经在用)。

### 4.3 remote_server 子命令(改动 2)

```rust
// crates/remote_server/src/server.rs
#[derive(Subcommand)]
pub enum Commands {
    // ... 现有的 Run / Proxy / Version / Exec
    #[cfg(unix)]
    #[command(subcommand)]
    AcpMux(acp_mux::Command),
}

// run():
#[cfg(unix)]
Commands::AcpMux(command) => acp_mux::run(command),
```

`crates/remote_server/src/main.rs:60` 的 usage 文本里同步加上 `acp-mux`。

### 4.4 命令包装钩子(改动 3、4)

```rust
// crates/project/src/agent_server_store.rs
pub type AgentCommandWrapper =
    Arc<dyn Fn(&str, AgentServerCommand) -> AgentServerCommand + Send + Sync>;

impl AgentServerStore {
    pub fn set_command_wrapper(&mut self, wrapper: Option<AgentCommandWrapper>) {
        self.command_wrapper = wrapper;
    }
}

// handle_get_agent_server_command 里,在 `.await?` 得到 command 之后、构造 proto 之前:
let command = match this.read_with(&cx, |this, _| this.command_wrapper.clone()) {
    Some(wrapper) => wrapper(&envelope.payload.name, command),
    None => command,
};
```

```rust
// crates/remote_server/src/headless_project.rs,AgentServerStore::local(...) 之后
if let Some(wrapper) = acp_mux::command_wrapper().log_err().flatten() {
    agent_server_store.set_command_wrapper(Some(wrapper));
}
```

`acp_mux::command_wrapper()` 读取 `acp_mux.json`。对启用了的 agent,返回的闭包把 `{path, args, env}` 改写成:

```
path = std::env::current_exe()
args = ["acp-mux", "attach", "--agent", <name>, "--", <原 path>, <原 args...>]
env  = 原 env(由 exec 设置到 attach 进程上,再通过握手传给 daemon)
```

**不要在 `exec` 里拦截。** `exec` 也被 MCP context server 使用(`crates/project/src/context_server_store.rs:980`)。

### 4.5 不改的部分

- Mac 端的 `transport.rs`、`ssh.rs`:命令改写对它们是透明的。
- remote_server 的连接管理、空闲超时、shutdown:daemon 和它们没有关系。

## 5. 不打补丁时的 Zed 行为(基线)

只上线改动 1–4,Mac 端用原版 Zed:

| 场景 | 表现 |
|---|---|
| 发起任务后断开,之后从线程历史重新打开 | 能看到完整内容,包括断开期间的内容(重放) |
| attach 时 agent 仍在执行 | 能看到实时增量,但**线程显示空闲**:没有停止按钮,结束时没有"完成"通知 |
| 断开期间产生的授权请求 | attach 后弹出,可以批准 |
| attach 时 agent 在执行,用户又发了一条消息 | daemon 先取消当前轮次,再发送新消息(第 3.6 节) |

## 6. Zed 的修改:attach 后正确显示正在执行的轮次

### 6.1 目标

attach 时如果 agent 有轮次正在执行,Zed 里的线程要表现得**和自己发起的轮次一样**:

- `status() == Generating`,显示生成中的指示和停止按钮;
- 点停止会发出 `session/cancel`,等到轮次真正结束;
- 用户此时发的新消息走 ThreadView 已有的排队逻辑;
- 轮次结束时发出 `AcpThreadEvent::Stopped`,触发完成通知、声音和发送排队的消息;
- 授权请求显示为 `WaitingForConfirmation`。

### 6.2 协议扩展(ACP 的 `_` 前缀扩展通知)

```jsonc
// daemon → Zed
{"jsonrpc": "2.0", "method": "_zed.dev/acpMux/turn_state",
 "params": {"sessionId": "...", "state": "running"}}
{"jsonrpc": "2.0", "method": "_zed.dev/acpMux/turn_state",
 "params": {"sessionId": "...", "state": "idle", "stopReason": "end_turn"}}   // end_turn | cancelled | max_tokens | refusal | error
```

- `running` 只在 `session/load` 重放结束、load 响应之前发送,而且只针对**不是由这个 client 发起**的轮次。
- `idle` 在 daemon 收到 agent 的 prompt 响应时,发给 observer,前提是这个 observer 之前收到过 `running`。如果轮次是这个 client 自己发起的,正常的 prompt 响应就够了,不发 `idle`。
- 只有当 `InitializeResponse` 里带有 `_meta["zed.dev/acpMux"]` 时,Zed 才会处理这个通知。不带的 agent 完全不受影响。

### 6.3 `acp_thread`:新增 `attach_external_turn`

思路:**复用 `run_turn` 的完成逻辑**。`run_turn` 的参数 `f` 本来是"等待 prompt 的响应";在这里换成"等待 daemon 发来的 idle 通知"。这样 Running/Idle 的切换、`Stopped` 事件、cancel、max_tokens 和 refusal 的处理全部沿用现成代码。

需要做的调整:

1. **把 `run_turn` 拆成两层**,避免为这个轮次注册一个 submission(否则"重试"会把一个空 prompt 当成最近一次提交):
   - `fn start_turn(&mut self, first_entry_index: Option<usize>, cx, f) -> Task<Result<Option<acp_v1::PromptResponse>>>`:包含现在 `run_turn` 从 `clear_completed_plan_entries` 到 `completion` 的全部逻辑。`first_entry_index` 为 `None` 时,保持现在的 `entries.len() - 1`。
   - `fn run_turn(&mut self, id, cx, f) -> Submission`:改成 `self.track_submission(id, cx, …start_turn(None, cx, f)…)`,行为和现在一样。
2. **新增公开方法:**

```rust
// crates/acp_thread/src/acp_thread.rs
/// Mirrors a turn that the agent started on behalf of another client connection,
/// so the thread shows it as running until `turn_end` resolves.
pub fn attach_external_turn(
    &mut self,
    turn_end: oneshot::Receiver<acp_v1::PromptResponse>,
    cx: &mut Context<Self>,
) {
    if self.running_turn.is_some() || self.uses_reported_activity() {
        return;
    }
    // The turn began before this connection attached, so its entries start at the
    // last user message rather than at the end of the replayed history.
    let first_entry_index = self.last_user_message().map(|(index, _)| index);
    self.start_turn(first_entry_index, cx, async move |_, _| {
        turn_end
            .await
            .map_err(|_| anyhow!("Lost connection to the agent while it was running"))
    })
    .detach();
}
```

3. **说明:**
   - `start_turn` 一开始会调用 `cancel_inner(InterruptedByFollowUp)`。这时没有运行中的轮次,它只会取消已有的授权请求,所以 daemon 必须**先发 `running`,再补发授权请求**(第 3.7 节)。
   - 如果 sender 被丢弃(连接断开),`turn_end` 返回 Err。`run_turn` 现有的 `Err(e)` 分支会处理:设回 Idle,标记 `had_error`,发出 `Error` 事件。UI 会显示"连接丢失",符合预期。
   - 点击停止时:`cancel_inner` 取出 `running_turn` → `connection.cancel` 发出 `session/cancel` → daemon 转发给 agent → agent 以 `cancelled` 结束 → daemon 发 `idle` → `turn_end` 完成 → `send_task` 结束。这和普通轮次的取消路径相同(`acp_thread.rs:6629-6665`)。

### 6.4 `agent_servers`:识别 mux 并处理通知

1. **识别**(`AcpConnection` 构造时,即解析 initialize 响应的地方):

```rust
// crates/agent_servers/src/acp.rs,struct AcpConnection 新增字段
supports_mux_turn_state: bool,

// 解析 InitializeResponse 时
supports_mux_turn_state: response
    .meta
    .as_ref()
    .and_then(|meta| meta.get("zed.dev/acpMux"))
    .is_some(),
```

2. **会话状态**(`struct AcpSession`,`acp.rs:464`):

```rust
external_turn: Option<oneshot::Sender<acp::PromptResponse>>,
```

3. **注册扩展通知**(在 `client_builder` 中,`acp.rs:713` 附近,其他 notification handler 后面)。

参考 `v2_terminal_client_builder`(`acp.rs:726-760`)的 `UntypedMessage` 写法:先匹配方法名 `_zed.dev/acpMux/turn_state`,解析出 `{sessionId, state, stopReason}`,然后用 `enqueue_notification` 交给前台处理,保证它和 `session/update` 按顺序执行。

```rust
fn handle_mux_turn_state(notification: MuxTurnState, cx: &mut AsyncApp, ctx: &ClientContext) {
    let session_id = acp_v2::SessionId::new(notification.session_id);
    let mut sessions = ctx.sessions.borrow_mut();
    let Some(session) = sessions.get_mut(&session_id) else {
        log::warn!("acp-mux turn state for unknown session {session_id:?}");
        return;
    };
    match notification.state {
        MuxTurnStateKind::Running => {
            let (sender, receiver) = oneshot::channel();
            session.external_turn = Some(sender);
            let thread = session.thread.clone();
            drop(sessions);
            thread
                .update(cx, |thread, cx| thread.attach_external_turn(receiver, cx))
                .log_err();
        }
        MuxTurnStateKind::Idle { stop_reason } => {
            if let Some(sender) = session.external_turn.take() {
                sender.send(acp::PromptResponse::new(stop_reason)).ok();
            }
        }
    }
}
```

- 只有在 `supports_mux_turn_state` 为 true 时才注册这个 handler,或者在 handler 里检查这个开关。`ClientContext` 需要能拿到这个开关,可以在构造时一起传进去。
- 会话被释放时(`remove_session`),丢弃 `external_turn`,让 `turn_end` 以 Err 结束,避免线程永远停在运行状态。

### 6.5 Zed 补丁的测试

在 `crates/agent_servers/src/acp.rs` 的测试里,用已有的 fake ACP 连接(`connect_fake_acp_connection`,`acp.rs:2662`)构造场景:

1. initialize 响应带上 `_meta["zed.dev/acpMux"]`。依次发送:load → 重放 update → `turn_state: running` → load 响应。断言 `thread.status() == Generating`。
2. 发送 `turn_state: idle, end_turn`,断言状态变回 `Idle`,并且收到 `Stopped` 事件。
3. 在 running 状态下调用 `thread.cancel()`,断言发出了 `session/cancel`;发送 idle(cancelled)后,cancel 的 task 完成。
4. 在 running 状态下关闭连接,断言变回 `Idle`,`had_error() == true`。
5. 先发 running,再发 `request_permission`,断言 `is_waiting_for_confirmation()` 为 true,并且授权请求没有被取消。
6. initialize 响应**不带** `_meta` 时,`turn_state` 通知被忽略(回归测试)。

## 7. 测试方案(acp-mux)

| 层 | 内容 |
|---|---|
| 单元测试 `snapshot.rs` | 每条压缩规则;tool_call 字段覆盖;不同 messageId 的消息块不合并;未知类型原样保留 |
| 单元测试 `router.rs` | id 映射;拦截 close;挂起和补发授权请求;observer 被新 client 挤掉;client 断开后,它发起的 prompt 继续执行 |
| 集成测试 | 用一个假 agent(按脚本输出 ACP 消息的小程序)和两个假 client 走完整流程:new → prompt → 断开 → agent 继续输出并请求授权 → 第二个 client load → 断言重放内容、`running` 通知、授权请求、`idle` 通知 |
| 端到端(手动) | Mac Zed + Linux + Claude Code:发起长任务 → 退出 Zed → 等 5 分钟 → 重新打开 → 线程显示"生成中"、可以停止,完成后有通知;`acp-mux list` 的输出和实际一致 |

## 8. 部署

1. 在 Linux 上执行 `./script/build-linux-remote-server.sh`,产物带有 `acp-mux` 子命令。
2. 在 Linux 上写好 `~/.config/zed/acp_mux.json`,设置 `enabled: true`,并列出要接入 mux 的 agent。
3. 把产物拷到 Mac,用 `ZED_COPY_REMOTE_SERVER` 启动 Zed 并连接,Zed 会自动上传(`docs/src/development/linux-remote-server-build.zh-CN.md`)。
4. 如果用到了第 6 节的补丁,在 Mac 上用 `./script/build-mac.sh --channel stable --with-copy-remote-server` 重新构建 Zed。
5. 可选:把 daemon 配置成 systemd user service(`ExecStart=<remote_server 路径> acp-mux daemon`),并执行 `loginctl enable-linger`,这样用户的所有会话都退出后 daemon 也不会被 systemd 清理掉。不用 systemd 的话,依靠 setsid,daemon 在 ssh 断开后也会继续运行;但部分发行版设置了 `KillUserProcesses=yes`,会在用户退出登录时杀掉它,需要确认。

## 9. 已知的限制和待验证项

| # | 项 | 说明 |
|---|---|---|
| 1 | 失去 Zed 的 action log | agent 不再通过 Zed 的 fs 能力写文件,所以"审阅、保留或撤销改动"不可用;工具调用里附带的 diff 仍然可以看到 |
| 2 | 失去内嵌终端 | 关闭了 terminal 能力,命令输出以工具调用内容的形式展示 |
| 3 | 断线后 Zed 是否自动重新加载 | ssh 断开后,attach 进程退出,线程会显示连接错误。需要实测重连后 Zed 是否会自动重新启动 agent 命令并 load;不会的话,要从历史里重新打开 |
| 4 | 自定义 agent 的配置位置 | `GetAgentServerCommand` 在 Linux 端解析命令,自定义 agent 可能需要写在 Linux 端的 Zed 设置里 |
| 5 | `session/list` 在 UI 里的入口 | 要确认 Zed 的线程历史对外部 agent 会调用 `session/list` 并允许打开(`AcpSessionList`,`acp.rs:473`) |
| 6 | 上游 ACP v2 的状态上报 | `AcpThread::update_session_state`(`acp_thread.rs:4200`)接入生产代码后,第 6 节的补丁可以换成 daemon 发标准的 `StateUpdate`,补丁就可以删除 |
| 7 | `KillUserProcesses` | 见第 8 节第 5 步 |
| 8 | 多个 Zed 窗口 | 每个窗口各有一个 `AcpConnection` 和一个 attach。同一个会话后 attach 的会挤掉先来的,先来的窗口里的线程会停止更新;可以考虑让 daemon 给被挤掉的一方发一条提示 |

## 10. 里程碑

| 阶段 | 内容 | 工作量 |
|---|---|---|
| M1 | `acp_mux` crate:attach、daemon、agent 进程管理、initialize 缓存、id 映射、透传;remote_server 子命令;命令包装钩子。能用,但还不能 detach | 3–4 天 |
| M2 | `MuxSession`、snapshot 压缩、`session/list`、`session/load` 重放、拦截 close、挂起和补发授权请求、observer 切换 | 4–5 天 |
| M3 | Zed 补丁(第 6 节)及测试;daemon 端的 `turn_state` | 2–3 天 |
| M4 | `list`、`kill`、`restart`;协议版本握手;持久化和 daemon 重启后恢复(可选);`auto_approve`;systemd 文档 | 3–4 天 |

合计约 **2.5–3 周**。M1+M2 完成后已经可以按 tmux 的方式使用;M3 完善"正在执行"状态的显示。

## 11. 实现进度(2026-10-17)

代码在分支 `ymzed` 的工作区里,**尚未提交**。

### 11.1 里程碑状态

| 阶段 | 状态 | 说明 |
|---|---|---|
| M1 | 已完成 | attach、daemon、agent 进程管理、initialize 缓存和改写、JSON-RPC id 映射、透传;remote_server 子命令;命令包装钩子 |
| M2 | 已完成 | `MuxSession`、snapshot 压缩、`session/list`、`session/load` 重放、拦截 close、挂起和补发授权请求、observer 切换 |
| M3 | 已完成 | Zed 补丁(第 6 节)及测试;daemon 端发送 `turn_state` |
| M4 | 部分完成 | 已完成:`list`、`kill`、`restart`,协议版本握手,`auto_approve`。未做:持久化和 daemon 重启后恢复(可选),systemd 文档 |

### 11.2 改动清单(对应 4.1 节)

| # | 位置 | 状态 |
|---|---|---|
| 1 | 新 crate `crates/acp_mux`(`src/acp_mux.rs`、`attach.rs`、`daemon.rs`、`agent.rs`、`session.rs`、`snapshot.rs`、`router.rs`、`control.rs`),只在 unix 上编译 | 已完成 |
| 2 | `crates/remote_server/src/server.rs`:`Commands::AcpMux`;`main.rs` 的 usage 文本;`Cargo.toml` 在 unix 下依赖 `acp_mux` | 已完成 |
| 3 | `crates/project/src/agent_server_store.rs`:`AgentCommandWrapper`、`set_command_wrapper`,在 `handle_get_agent_server_command` 返回前应用 | 已完成 |
| 4 | `crates/remote_server/src/headless_project.rs`:`acp_mux_command_wrapper()` 读取 `acp_mux.json` 并设置钩子 | 已完成 |
| 5 | `crates/agent_servers/src/acp.rs`、`acp/v1.rs`:识别 `_meta["zed.dev/acpMux"]`;`ClientContext.mux_turn_state`;`AcpSession.external_turn`;注册 `_zed.dev/acpMux/turn_state` 处理函数;断开连接时丢弃外部轮次的 sender | 已完成 |
| 6 | `crates/acp_thread/src/acp_thread.rs`:`run_turn` 拆出 `start_turn`,新增 `attach_external_turn` | 已完成 |

另外在根 `Cargo.toml` 的 workspace members 和 dependencies 里加了 `acp_mux`。

### 11.3 和本文设计的差异

| # | 本文的设计 | 实际实现 | 原因 |
|---|---|---|---|
| 1 | `attach_external_turn(turn_end: oneshot::Receiver<PromptResponse>)` | 接收 `oneshot::Receiver<Result<PromptResponse>>` | daemon 发 `stopReason: "error"` 时,线程显示为出错,而不是编造一个停止原因 |
| 2 | `acp_mux::command_wrapper()` 返回闭包 | `acp_mux` 提供 `Config::load`、`Config::wraps_agent` 和 `wrap_command`,闭包在 `headless_project.rs` 里组装 | `acp_mux` 不依赖庞大的 `project` crate,编译和测试都轻 |
| 3 | 按 `(agent 名, 命令哈希)` 复用 agent | 哈希只包含 program、args,以及 agent 显式配置的环境变量的值(包装时以 `--key-env KEY` 传给 attach) | 完整环境里有 `SSH_CLIENT` 这类每次连接都不同的变量,会导致每次 attach 都新起一个 agent |
| 4 | — | stdin 是终端时,attach 直接 exec 真实命令,不接入 mux | 终端登录(terminal auth)复用 agent 命令,但需要交互式终端,不能接到共享的 agent 上 |
| 5 | 配置里列出要接入的 agent | 没有 `agents` 字段时,所有外部 agent 都接入;有则只接入列出的 | 兼顾第 2 节"所有外部 agent 自动接入"和配置示例 |
| 6 | 改写 initialize 响应时设置 `loadSession`、`list`,删除 `resume` | 另外宣告 `sessionCapabilities.close = {}` | Zed 只有在 agent 支持 close 时,才会在释放线程时发 `session/close`;daemon 拦截后只解除 observer |
| 7 | 会话不存在时 `session/load` 返回 "session not found" | 如果 agent 本身支持 `loadSession`,转给 agent 加载,并把它重放的内容记进 snapshot;否则返回 `-32002 Session not found` | 能打开接入 mux 之前、或 daemon 重启之前由 agent 自己持久化的会话 |
| 8 | `pending_agent_requests` 放在 `MuxSession` 里 | 放在 agent 状态里,每条带 `session_id` | 也要处理不属于任何会话的请求(例如 request 作用域的 elicitation),对外行为一样 |
| 9 | 有轮次在执行时收到 `session/prompt`:先 cancel,等结束后转发 | 同左;等待中的 prompt 进入 `queued_prompts`,轮次结束后依次发送 | — |
| 10 | — | 关停时,daemon 等 `restart` 的回复写完才退出(新增 `Event::Exit`) | 否则 `restart` 可能收不到回复 |
| 11 | — | agent 进程放进独立的进程组,`kill --agent` 和关停时对整个进程组发 SIGTERM | agent 往往会再起子进程(例如 claude-code-acp 起 claude CLI) |
| 12 | — | agent 退出后,daemon 关闭连到这个 agent 实例的所有 client 连接 | Zed 能看到 agent 退出;下次启动时 attach 会拉起新的 agent 进程 |

### 11.4 未实现的部分

- 持久化 snapshot 和 daemon 重启后恢复(3.7 节末尾、M4,可选)。daemon 重启后,只能通过差异 7 由 agent 自己 load。
- systemd user service 的示例和文档(第 8 节第 5 步)。
- ACP v2 连接(`AcpV2Connection`)没有注册 `turn_state` 处理函数;目前只有 v1 连接会处理。
- 被挤掉的 client 不会收到提示(第 9 节第 8 项);它挂着的授权请求会收到 `$/cancel_request`。
- `session/list` 不分页,一次返回全部会话。

### 11.5 测试情况

| 范围 | 内容 | 结果 |
|---|---|---|
| `cargo test -p acp_mux` | 28 个测试:snapshot 的每条压缩规则;session 的 load 响应和 prompt 记录;initialize 改写;router 的 id 映射、拦截 close、`session/list`、重放和 `running`、发起者自己不收 `running`、observer 被挤掉和授权请求转移、client 断开后轮次继续、取消后再发 prompt、`auto_approve`、会话不存在、agent 退出;CLI 参数解析;以及一个真实 unix socket 的端到端测试(用 `sh` 写的假 agent:第一个 client 发 prompt 后断开,第二个 client load,依次断言重放内容、`running`、load 响应、补发的授权请求、回答后的 `idle`) | 全部通过 |
| `agent_servers` 新增测试(6.5 节) | 5 个测试覆盖 6 个场景:attach 后显示 Generating,`idle` 后变回 Idle 并发出 `Stopped`;取消要等 agent 报告结束才完成;连接断开后变回 Idle 且 `had_error`;`running` 之后到达的授权请求保持等待确认;不带 `_meta` 的 agent 忽略 `turn_state` | 全部通过 |
| 回归 | `cargo test -p acp_thread -p agent_servers`(282 + 142 个) | 全部通过 |
| clippy | acp_mux、agent_servers、acp_thread、project、remote_server,`--deny warnings` | 无警告 |
| 未做 | remote_server 自身的测试套件;第 7 节的手动端到端(Mac Zed + Linux + Claude Code) | — |

注:在这台 Mac 上编译 remote_server 时,`gpui_apple` 的 Metal shader 编译会失败(本机环境问题,与本次改动无关),check 和 clippy 时加了 `-p gpui_apple --features gpui_apple/runtime_shaders` 绕过。

### 11.6 下一步

1. 在 Linux 上用 `./script/build-linux-remote-server.sh` 构建,写好 `~/.config/zed/acp_mux.json`,按第 7 节做手动端到端测试。
2. 实测第 9 节第 3、4、5 项(断线后是否自动重新加载、自定义 agent 的配置位置、线程历史里 `session/list` 的入口)。
3. 根据实测结果决定是否做持久化(M4 可选项)和 systemd 文档。
