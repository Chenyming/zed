# Zed Agent 的 attach/detach(类 tmux)方案调研

> 调研方式:对源码只读调研(分支 `ymzed`,2026-10-16)。
> 需求:Linux 上运行 Zed server,macOS 上运行 Zed client。client 通过 SSH、HTTP 或 WebSocket 连接后,触发一个 agent 任务,然后断开连接;server 上的 agent 继续运行。之后 client 可以随时重新连上来(attach)查看状态,也可以随时断开(detach),行为模式和 tmux 一样。
> 相关文档:`ui_mirror_plan_c.md`、`ui_mirror_plan_d.md`、`ui_mirror_cross_platform.md`

## 1. 结论

1. **Zed 现有的 SSH 远程开发做不到这一点。** 在远程项目里,agent 的运行时都在 **Mac 客户端**一侧:
   - Zed 内置 agent(NativeAgent)的线程状态、LLM 请求和工具调度,全在客户端的 Zed 进程里;`remote_server` 不加载 agent(`crates/remote_server/src/headless_project.rs` 中只有 `AgentServerStore`)。
   - 外部 ACP agent(Claude Code、Codex 等):Linux 端只负责把命令解析出来(`GetAgentServerCommand`,`crates/project/src/agent_server_store.rs:869`);实际的进程是 **Mac 客户端**通过 `remote_client.build_stdio_command` 包成一条 ssh 命令启动的(`crates/agent_servers/src/acp/transport.rs:35-63`)。agent 进程的 stdio 挂在客户端的 ssh 子进程上,客户端一断开,ssh 退出,agent 也就收到 EOF 结束了。
   - `remote_server` 本身在断开后会再等 10 分钟,没有新连接就退出(`crates/remote_server/src/server.rs:520-560`)。它保住的只是项目、LSP 和 buffer 的状态,agent 不在里面。
2. **要实现 tmux 式的行为,agent 运行时必须常驻在 Linux 上,并且和客户端连接的生命周期分开。** 有三条路线,下面按投入从小到大排列。

| 路线 | 思路 | 支持哪些 agent | Mac 端 | 改 Zed 的量 | 工作量 |
|---|---|---|---|---|---|
| **R1:ACP 多路复用守护进程(推荐先做)** | 在 Linux 上运行一个 `acp-mux` 守护进程,长期持有 agent 进程;Zed 每次连接只是起一个 `acp-mux attach` 薄客户端 | 外部 ACP agent(Claude Code、Codex、Gemini CLI 等) | 原版 Zed | **不改**(只改配置) | 1.5–2.5 周 |
| R2:把 Zed 内置 agent 做成无界面的 ACP server | 写一个无窗口的 GPUI 程序,把 `NativeAgent` 包装成 ACP agent,再挂到 R1 的守护进程下 | 再加上 Zed 内置 agent | 原版 Zed | 新增一个二进制 crate,不改现有代码 | 在 R1 基础上再加 2–4 周 |
| R3:Linux 上运行完整的 Zed(GUI 或 Xvfb)加方案 C/D | server 就是一个常驻的 Zed 应用,agent 状态都在它的 `AgentPanel` 里 | 全部 | 方案 C 的镜像客户端或方案 D 的网页客户端,**不是原版 Zed** | 新增 crate | 见方案 C/D 文档 |

如果"client 就用原版 Zed"是硬性要求,应该走 **R1,必要时加上 R2**。

## 2. R1:ACP 多路复用守护进程

### 2.1 结构

```
macOS Zed ──ssh(Zed 远程项目自带)──> Linux: acp-mux attach <name>  ──unix socket──>  acp-mux daemon ──stdio──> claude-code-acp / codex-acp ...
   (可以随时断开)                       (随连接创建和退出)                              (常驻,类似 tmux server)          (常驻)
```

- 守护进程在第一次 attach 时自动拉起,之后用 systemd user service 或 `setsid` 让它常驻,和 tmux server 的做法一样。
- Mac 上的 Zed 打开一个 SSH 远程项目,在 `agent_servers` 里配置一个自定义 agent,命令是 `acp-mux attach claude`。因为是远程项目,Zed 会自动在 Linux 上执行这条命令(`transport.rs:35-47`),**Zed 一行代码都不用改**。
  - 要确认的一点:自定义 agent 的命令解析发生在 Linux 端(`handle_get_agent_server_command`),这个 agent 可能需要写在 Linux 端的 Zed 设置里。需要实际测试。
- 不用远程项目也可以:在 Mac 的本地项目里把命令配置成 `ssh host acp-mux attach claude`。但这样就看不到 Linux 上的文件了,不推荐。

### 2.2 守护进程要做的事

ACP 是"一个客户端对一个 agent"的 JSON-RPC 协议,`acp-mux` 要在中间扮演两个角色:

| 对 agent 扮演 | 对 Zed 扮演 |
|---|---|
| 一个**永远在线**的客户端 | 一个 agent |

1. **缓存初始化结果。** 只在启动时对真实的 agent 做一次 `initialize`。之后每个 attach 上来的 Zed 发来的 `initialize`,都用缓存的 `agent_capabilities` 回答;同时**必须宣告支持 `load_session` 和 `session/list`**。Zed 已经支持这几个能力(`crates/agent_servers/src/acp.rs:1798-1870`,以及 `AcpSessionList` 在 `:473`)。
2. **会话列表和重新 attach。** Zed 的线程历史会通过 `session/list` 列出守护进程里的会话;用户打开其中一个时,Zed 会发 `session/load`(或 `session/resume`)。守护进程把这个会话**从开头到现在**的全部 `session/update` 通知重放一遍(守护进程自己维护每个会话的日志),再把这个连接设为该会话的当前观察者,后续的增量直接转发。这就是"attach 之后看到全部状态"。
3. **detach 期间,agent 发给客户端的请求由守护进程处理**:
   - `session/request_permission`:**挂起**,等下次 attach 时再转发给 Zed,和 tmux 里的程序等你回来输入一样。也可以在守护进程里配置自动策略,比如对读操作自动允许。
   - `fs/read_text_file`、`fs/write_text_file`、`terminal/*`:Zed 默认会宣告这些客户端能力(`acp.rs:764-787`)。守护进程转发给 agent 的 `initialize` 里应当**去掉这些能力**,让 agent 在 Linux 上直接用自己的工具读写文件、执行命令。这样不管有没有连接,行为都一致。
     - 代价:Zed 的 action log(审阅、保留或撤销 agent 的改动)拿不到这些编辑。不过 Claude Code 等 agent 会在工具调用里附带 `Diff` 内容,Zed 线程里仍然能看到 diff。
4. **处理正在进行中的轮次。** 如果 attach 的时候 agent 正在回复(`session/prompt` 的请求还没有结果),那个请求 id 属于上一个连接。守护进程要自己吃掉最终的响应,只把 `session/update` 转发给新连接。
   - 需要验证:Zed 在 `load_session` 之后收到"不是自己发起的轮次"的更新时,线程状态(生成中、已停止)会不会显示错误。如果有问题,可以在守护进程里先把这一轮发完,再标记 load 完成;或者给 Zed 打一个很小的补丁。
5. **同一个会话有多个 Zed 同时 attach**:第一版只允许一个观察者,后来的连接把前一个踢掉,类似 `tmux attach -d`。
6. **取消和结束会话**:`session/cancel` 直接透传;Zed 关闭线程时发出的 `close_session`(`acp.rs:193`)要被守护进程拦截,**不能**真的转发给 agent,否则 detach 就变成了 kill。真正结束会话要通过 `acp-mux kill <session>` 这样的显式命令。

### 2.3 需要实测的风险

- Zed 在 ssh 断开再重连后,对外部 agent 的处理是:连接已退出时显示错误,需要用户手动重试,还是自动重新启动 agent 命令?如果需要手动,第一版可以接受。
- 各家 ACP agent 对 `load_session` 的支持程度不同。但这一点由守护进程自己实现(重放日志),**不依赖** agent 本身是否支持 load。
- 环境变量和认证:API key 和 `claude login` 的凭据要放在 Linux 上;守护进程常驻时继承的环境和 Zed 启动 agent 时不同,要显式配置。

## 3. R2:让 Zed 内置 agent 也能 detach

- 现状:仓库里没有 `impl acp::Agent`(Zed 只实现了 ACP 的客户端);`NativeAgent` 只能在 Zed 进程里通过 `NativeAgentConnection`(实现的是 `AgentConnection`)使用。
- 做法:新增一个无窗口的二进制 `zed-agent-acp`,在无界面的 GPUI App 里初始化 settings、language models、`Project::local`,以及 `agent::NativeAgent`;对外把 ACP 的 `session/new`、`prompt`、`cancel` 等方法映射到 `NativeAgentConnection`,再把 `AcpThread` 的事件转换成 `session/update`,和方案 D 里"从数据层读取"的逻辑相同。然后把它挂到 R1 的守护进程下面,Mac 上的 Zed 就把它当成一个外部 agent 来用。
- 难点:登录和模型凭据、MCP 和上下文服务器、工具权限设置(要读 Linux 上的 settings),以及 `agent` crate 的修改频率很高(3 个月 71 次提交)。

## 4. R3:Linux 上跑完整的 Zed

- Linux 上一直开着 Zed 的 GUI(或者在 Xvfb 里运行);agent 线程活在 `AgentPanel` 里,不会因为客户端断开而停下。attach 和 detach 就是方案 C 镜像客户端或方案 D 网页客户端的连接和断开。
- 这是对 agent 功能覆盖最全的方案(内置 agent 加 action log),但 Mac 端不是原版 Zed。方案 D 文档的坑 1 和坑 2 同样适用:线程必须已经被 `AgentPanel` 加载,并且至少要开着一个窗口。

## 5. 建议

1. 先做 **R1 的最小可用版本**:守护进程加 attach 薄客户端;缓存 `initialize`;实现 `session/list` 和 `load`(重放日志);挂起 detach 期间的权限请求;拦截 `close_session`。用 Claude Code ACP 验证"发起任务 → 断开 → 重新连接后看到完整过程,并且能批准挂起的权限请求"这一整条链路。约 1.5 周。
2. 验证通过后,再决定是否需要 R2(内置 agent)。如果平时主要用 Claude Code、Codex,R1 就够了。
3. R1 的守护进程可以同时作为方案 D 的后端。它已经掌握了全部会话状态,加一个 WebSocket 出口,手机网页端就能 attach 上去看,并且收到推送。

## 6. 重新 attach 后能否看到最新状态(补充调研)

**结论:内容能完整拿到;只有一项不能自动同步,就是"agent 是否仍在运行"这个状态。它要么靠守护进程绕开,要么给 Zed 打一个很小的补丁。**

### 6.1 能拿到的

| 状态 | 机制 | 依据 |
|---|---|---|
| 断开期间产生的全部消息、工具调用、diff | Zed 发 `session/load` 之前就已经创建并登记了 `AcpThread`,所以 load 过程中守护进程重放的 `session/update` 都会写进这个线程 | `crates/agent_servers/src/acp.rs:1275-1288`(`open_or_create_session` 先调用 `register_session`,再发 RPC) |
| 用户自己发过的消息 | 守护进程把它们以 `UserMessageChunk` 的形式重放,Zed 会建出对应的条目 | `crates/acp_thread/src/acp_thread.rs:4317` |
| attach 之后的实时增量 | load 完成后,守护进程继续把这个会话的 `session/update` 转发给新连接 | 同上 |
| 断开期间挂起的授权请求 | 守护进程把挂起的 `session/request_permission` 重新发给新连接。Zed 处理授权请求时只按 session id 找到线程,不检查当前是否在运行轮次 | `acp.rs:7414`,`acp_thread.rs:5853` |
| Zed 重启后自动恢复 | Zed 会持久化线程元数据,重新打开线程时调用 `load_session`,相当于自动 attach | `ThreadMetadataStore` |

### 6.2 不能自动同步的:"正在运行"指示

- 对外部 ACP agent,Zed 走的是旧的状态模型:只有 **Zed 自己发出的 `prompt`** 才会把线程置为 `Running`(`acp_thread.rs:6465`);`sync_legacy_action_state` 在 `Idle` 状态下直接返回,不做任何事(`acp_thread.rs:4178`)。
- 所以,如果 attach 时 agent 还在执行断开前发起的那一轮,Zed 里**会实时看到新内容不断出现,但线程显示为空闲**:没有"生成中"的指示和停止按钮,轮次结束时也不会触发"完成"通知(`Stopped` 事件)。
- 代码里已经有 ACP v2 的"由 agent 上报状态"路径(`update_session_state`,`acp_thread.rs:4200`,`StateUpdate::Running/Idle/RequiresAction`),它能完全解决这个问题。但目前**只在测试里调用**,`agent_servers` 的生产代码还没有接上(`receipt_transport` 没有被设置)。上游接通以后,守护进程只要发出 `StateUpdate` 就行了。

**在那之前的处理办法:**

1. **守护进程层面(不改 Zed):**
   - attach 时如果有正在运行的轮次,在开头插一条提示,例如"⏳ 任务仍在运行",轮次结束后再插一条"✅ 已完成"。
   - Zed 这时认为线程空闲,用户可能会发新消息。守护进程收到 `session/prompt` 后,先排队,等当前轮次结束再转发给 agent;也可以直接返回"正在运行"的错误。
   - "取消"按钮不可用,就用一条特殊消息代替,例如 `/cancel`,守护进程收到后转换成 `session/cancel`。
2. **给 Zed 打小补丁(约 1–2 天):** 让 load 的响应可以在 `_meta` 里带上"当前有轮次在运行"。Zed 读到后调用 `set_foreground_state(Running)`,然后等守护进程发来一个自定义的"轮次结束"通知,再切回 `Idle` 并发出 `Stopped` 事件。等上游接通 v2 状态上报后,把这个补丁换成标准做法即可。

### 6.3 守护进程实现时要注意的地方

- **日志要压缩。** `tool_call_update` 是对字段的增量修改,`agent_message_chunk` 是逐个 token 的片段。长会话如果原样重放,会非常慢。守护进程应该维护一份"当前状态":每条消息合并成一个完整片段,每个工具调用保留最终的字段。attach 时重放这份当前状态,再接上实时增量。
- **断线时 Zed 端的表现。** ssh 一断,attach 薄客户端退出,Zed 里正在显示的线程会进入"连接已断开"的错误状态。需要实测重连后 Zed 会不会自动重新启动 agent 命令并 load;不会的话,就从线程历史里重新打开一次。
- **attach 之后会丢失的东西。** Zed 每次 load 都会新建 `AcpThread`,所以滚动位置、展开或折叠状态,以及 Zed 本地的 action log(待审阅的改动)都会丢失。前两项无关紧要;action log 在第 2.2 节去掉文件系统能力时就已经放弃了。

## 7. acp-mux 能否直接实现在 remote_server 里

**结论:不建议把它放进 remote_server 进程本身;建议做成 remote_server 二进制里的一个子命令,作为独立的常驻进程运行。这样复用 Zed 已有的部署方式,而且只需要在服务端改一处,所有外部 agent 就都能 detach,Mac 端的 Zed 一行不改。**

### 7.1 为什么不放在 remote_server 进程里:生命周期和 tmux 的语义相冲突

| remote_server 现有行为 | 依据 | 和 detach 的冲突 |
|---|---|---|
| 每个 Zed workspace 对应一个独立的 server 进程,标识是 `workspace-{id}` | `crates/remote/src/remote_client.rs:350-379`,`crates/remote_server/src/server.rs:881` | 会话被绑在某一个 workspace 上;换一个窗口、换一台 Mac,或者 workspace id 变了,都找不到原来的会话 |
| **新建连接**(不是断线重连)时,如果发现同一个标识的 server 还在运行,会**直接杀掉**再重新启动 | `server.rs:1001-1006` | Zed 重启或者重新打开项目,都算新建连接,agent 会被一起杀掉。只有网络抖动时的自动重连能保住 |
| Zed 退出或者关闭项目时,会发送 `ShutdownRemoteServer` | `crates/project/src/project.rs:1638`(退出应用)、`:2044`(Project 释放) | 正常地"关掉 Zed"就等于 kill,而不是 detach |
| 断开后 10 分钟内没有新连接就退出 | `server.rs:520-560` | 长任务跑不完 |
| 升级 Zed 后会使用新版本的 server 二进制 | 版本化的部署路径 | 新连接走新版本,旧进程里的会话就成了孤儿 |

上面的每一条都可以改,例如有活跃会话时不杀进程、把 shutdown 当作 detach、取消空闲超时。但这样要改 remote_server 的核心连接逻辑,而且"会话应该在整台主机范围内共享"这一点,和"每个 workspace 一个 server"的模型本身就对不上。

### 7.2 推荐做法:同一个二进制,独立的守护进程

```
Mac Zed ──ssh──> remote_server(workspace-N,随连接创建和销毁)
                    │  GetAgentServerCommand 返回的命令被改写成:
                    │    <remote_server 自身路径> acp-mux attach --agent claude -- <真实命令…>
                    ▼
                 acp-mux attach(由 Zed 通过 ssh stdio 启动的薄客户端)──unix socket──> acp-mux daemon(整台主机一个,常驻)──> agent 进程
```

1. **作为子命令。** 在 `crates/remote_server/src/server.rs` 的命令枚举里加上 `AcpMux { Daemon | Attach | List | Kill }`。daemon 是独立进程:用 `setsid` 脱离父进程,有自己的 pid 文件和 socket(放在 `paths::remote_server_state_dir()/acp-mux/` 下),不受 workspace server 的杀进程逻辑、shutdown 和空闲超时影响。
2. **只在服务端改一处,所有外部 agent 都能 detach。** 修改 `AgentServerStore::handle_get_agent_server_command`(`crates/project/src/agent_server_store.rs:613`):只在 remote_server 中启用(它通过 `downstream_client` 判断自己是不是在为远程客户端服务),并加一个设置开关。把解析出来的真实命令包装成 `current_exe() acp-mux attach --agent <name> -- <path> <args>`。Mac 端的 `transport.rs` 照常通过 ssh 去执行这个命令,**客户端完全不用改**。ACP 注册表里的 agent 和自定义 agent 都会自动走 mux。
3. **复用 Zed 已有的部署。** remote_server 二进制本来就是 Zed 自动上传的,不需要另外安装;Linux 上的 agent 下载和版本管理也继续由 `AgentServerStore` 负责。
4. **版本兼容。** Zed 升级后,新版本的 `attach` 会连接到旧版本的 daemon。attach 和 daemon 之间只传 ACP 的 JSON 行,再加一个很小的控制握手(带版本号),这样的格式比较稳定。Linux 上旧的二进制文件被删除,也不影响正在运行的 daemon 进程。需要升级 daemon 时,提供 `acp-mux restart`,会中断正在运行的任务,要由用户显式执行。

### 7.3 放在同一个仓库里的额外收益和代价

- **收益:**
  - 可以复用 `ShellBuilder`、`ProjectEnvironment`(加载登录 shell 的环境变量,解决第 2.3 节说的"常驻进程环境不对"的问题)和 `paths`。
  - 以后如果想让 detach 期间的文件读写经过 Zed 的 buffer 层,也可以在 daemon 里起一个无界面的 `HeadlessProject`。第一版不需要。
- **代价:** 要维护 fork 的范围变大了,但改动集中在 `server.rs` 的子命令和 `agent_server_store.rs` 的一处包装,和上游合并时冲突很少。daemon 的主体代码可以单独放进一个 crate(例如 `acp_mux`),remote_server 只负责调用它。

## 8. acp-mux 如何上传和部署

**结论:不需要单独上传。** acp-mux 作为 remote_server 二进制的子命令,会跟着 remote_server 一起,走本 fork 已有的 `ZED_COPY_REMOTE_SERVER` 流程上传(详见 `docs/src/development/linux-remote-server-build.zh-CN.md`)。

### 8.1 流程

1. 在 Linux 上执行 `./script/build-linux-remote-server.sh`,产物 `target/<triple>/release/remote_server` 已经包含 `acp-mux` 子命令。
2. 把产物复制到 Mac,用 `ZED_COPY_REMOTE_SERVER=<路径> ZED_BUILD_REMOTE_SERVER=never ZED_REMOTE_SERVER_VERSION= …/zed` 启动 Zed。连接时 Zed 会自动上传到 `~/.zed_server/zed-remote-server-<channel>-<version>`,并做 SHA-256 校验和缓存(`crates/remote/src/transport/ssh.rs:1055-1110`,`crates/remote/src/transport/copied_server.rs`)。
3. **只改了服务端,Mac 端 Zed 不需要重新构建。** 当前安装的 Zed 只要带有 `--with-copy-remote-server` 能力,并且已经支持 `exec` 就够了。原因是外部 agent 本来就是通过 `<remote_server> exec` 启动的:`build_stdio_command` 会把 `RemoteCommand` 编码后从 stdin 传给 `exec`(`ssh.rs:503-541`,`crates/remote_server/src/server.rs:157`)。第 7.2 节把命令改写成 `program = current_exe(), args = ["acp-mux", "attach", …]` 之后,`exec` 执行的仍然是同一个二进制。

### 8.2 注意

- **钩子放在 `agent_server_store` 里,不要放在 `exec` 里。** `exec` 同时被 MCP context server 使用(`crates/project/src/context_server_store.rs:980`),在 `exec` 里统一拦截会把 MCP server 也接管掉。
- **替换二进制不影响正在运行的 daemon。** `install_script` 用 rename 原子替换文件,daemon 继续使用旧文件的 inode。新的 `attach` 由新二进制执行,和旧 daemon 之间靠握手里的版本号判断兼容性(第 7.2 节第 4 条)。daemon 不会自动重启;要升级时执行 `remote_server acp-mux restart`,这会中断正在运行的任务。
- **daemon 不要保存或依赖自身二进制的路径。** 二进制被替换后,`/proc/self/exe` 会指向一个已删除的文件。需要拉起 daemon 时,一律由 `attach` 用它自己的 `current_exe()` 去启动。
- **如果改成独立的二进制**(不放进 remote_server):`ZED_COPY_REMOTE_SERVER` 不会帮你上传它,需要自己 `scp` 到例如 `~/.local/bin/acp-mux`,再在 Linux 端的 Zed 设置里把自定义 agent 的命令配置成它。这样做简单,但每个 agent 都要手动配置,注册表里的 agent 也不会自动接入 mux。不推荐。
