# Zed 远程 Agent 控制:方案 D(Agent 语义层 API)结论

> 调研方式:对 Zed 源码只读调研(分支 `ymzed`,2026-10-16)。
> 目标:在家里电脑运行 Zed,外出时通过手机等客户端查看并操作 agent 面板和线程,包括发消息、批准工具调用、取消、切换线程等。
> 相关文档:[方案 C(GPUI 绘制指令流)](./ui_mirror_plan_c.md)

## 1. 结论

- **可行性高,估计在 85% 以上。** 列出线程、实时查看流式输出、发消息、批准或拒绝工具调用、取消、切换和新建线程,这些核心功能用现有的公开 API 都能实现,不用改 Zed 现有代码,只需新增一个 crate。
- 真正的难点有两个:
  - **写操作走哪一层。** 要不要经过 UI 层,决定了行为是否和桌面一致。
  - **维护成本。** 2026-07-16 到 2026-10-16 的 3 个月里,`agent_ui` 有 121 次提交,`acp_thread` 有 51 次,`agent` 有 71 次,`sidebar` 有 33 次;作为对比,方案 C 依赖的 `gpui/src/scene.rs` 只有 3 次,`gpui/src/platform.rs` 有 32 次。方案 D 开发更简单,但长期维护比 C 更费力。

## 2. 读和写各走哪一层

| 层 | 关键 API(都是公开的) | 用途 |
|---|---|---|
| **数据层:`AcpThread` 实体**(`crates/acp_thread/src/acp_thread.rs:3418`) | `entries()`、`status()`、`title()`、`token_usage()`、`plan()`、`pending_permission_requests()`、`is_waiting_for_confirmation()`;事件 `AcpThreadEvent`(`NewEntry`、`EntryUpdated(idx)`、`StatusChanged`、`ToolAuthorizationRequested`、`Stopped`……,见 `:3559`);每类条目都有 `to_markdown(cx)`(`:368`、`:947`、`:1724`) | **读取。** 订阅事件,把状态推给客户端 |
| **UI 层:`ThreadView` / `ConversationView`**(`crates/agent_ui/src/conversation_view/thread_view.rs`) | `send_content(contents_task, …)`(`:1972`)、`authorize_permission_request(…, window, cx)`(`:2944`)、`cancel_generation`(`:2314`)、`MessageEditor::set_message`(`crates/agent_ui/src/message_editor.rs:1700`) | **写入。** 和桌面上用户亲手操作的行为一致 |

**写操作不要直接调 `AcpThread::send`**(`acp_thread.rs:6286`)。`ThreadView::send_content` 在调用 `thread.send` 之外,还做了这些事:应用当前选择的模型和模式、处理排队、跟随 agent、上报统计、维护加载状态。绕过它,桌面端的 UI 状态会和实际不一致。

**推荐的组合:** 读取走数据层,写入走 UI 层。要拿到 UI 层的对象,先找到 `Workspace` 里的 `AgentPanel`,再拿到对应的 `ConversationView` 和 `ThreadView`;调用时用 `cx.update_window(handle, …)` 获取 `Window`。

## 3. 功能逐项评估

| 功能 | 实现途径 | 难度 |
|---|---|---|
| 线程列表(标题、项目、时间、是否已归档) | `ThreadMetadataStore::global(cx).entries()`(`crates/agent_ui/src/thread_metadata_store.rs:601`),`ThreadMetadata` 定义在 `:309`;侧边栏就是用它 | 低 |
| 线程实时状态(运行中、等待批准、已完成) | 参考 `crates/sidebar/src/sidebar.rs:1462` 里 `live_thread_statuses` 的计算方式 | 低 |
| 查看消息流(MVP 版) | 每个条目调用 `to_markdown(cx)`,收到 `EntryUpdated` 时发送节流后的增量 | 低 |
| 查看消息流(结构化版) | 用户消息的 ContentBlock、`Markdown::source()`(`crates/markdown/src/markdown.rs:970`)、ToolCall 的标题、类型、状态和内容 | 中 |
| 发送纯文本消息 | 在空闲时调用 `ThreadView::send_content` | 低 |
| 忙碌时排队发送 | 排队逻辑在 `send`(`thread_view.rs:1776`)和 `queue_message`(`:2434`)里,而且和消息编辑器耦合 | **中**(见坑 3) |
| 工具调用的批准、拒绝、"始终允许" | 调用 `ThreadView::authorize_permission_request`,用 `PermissionRequest` 里的选项渲染按钮 | 低到中 |
| 回答 elicitation 表单 | `respond_to_elicitation`(`acp_thread.rs:6135`),表单由 JSON schema 描述,需要在客户端动态渲染 | 中 |
| 取消当前生成 | `ThreadView::cancel_generation` | 低 |
| 打开历史线程或新建线程 | `AgentPanel::load_agent_thread`(`crates/agent_ui/src/agent_panel.rs:4496`)、`create_thread_with_options`(`:3232`);可以参考侧边栏的 `load_agent_thread_in_workspace`(`sidebar.rs:3791`) | 中(见坑 1) |
| 查看 diff | 工具调用里的 `Diff` 实体(`crates/acp_thread/src/diff.rs`)有 `to_markdown`、`base_text` 和 `buffer`,可以拼出 unified diff | 中 |
| 保留或撤销 agent 的改动 | 通过 `thread.action_log()` 实现"全部保留"或"全部撤销";逐块审阅要复刻 `agent_diff` 的逻辑 | 中到高 |
| 实时查看终端输出 | `Entity<Terminal>` 是活的对象,需要节流后发送文本快照 | 中 |
| 切换模型和模式 | `ConfigOptionsUpdated`、`ModeUpdated` 事件加 ConversationView 里的选择器 | 中 |
| @文件引用 | 要另外暴露项目文件的模糊搜索,再组装成 `ResourceLink` 内容块;MVP 可以先不做 | 中 |
| 子 agent、上下文压缩、notice | 条目和事件都有,推送出去就行 | 低到中 |

额外的好处:Zed 自带的 agent 和外部 ACP agent(Claude Code、Codex 等)都统一走 `AcpThread`,所以一套实现能覆盖所有 agent,SSH 远程项目里的线程也包括在内。

## 4. 架构与协议

- **代码组织:** 新增一个 crate,比如 `agent_remote`,依赖 `agent_ui`、`acp_thread` 和 `workspace`,在 `zed` 启动时初始化成全局单例。
- **传输层:** 依赖里已经有 `async-tungstenite` 和 `tiny_http`(根目录 `Cargo.toml:558,886`),不需要新增第三方依赖就能起 WebSocket 服务。
- **消息格式:直接复用 ACP 的数据结构。** 采用 ACP 现有的 serde 类型,比如 `ContentBlock`、`ToolCall`、`SessionUpdate`、`RequestPermission`,再加几个扩展方法:
  - `zed/listThreads`、`zed/threadStatus`:线程列表和实时状态
  - `zed/openThread`、`zed/newThread`:打开或新建线程
  - `zed/snapshot`:断线重连时的全量快照
- **好处:** 客户端解析消息流的逻辑可以照搬 ACP 客户端,不用自己发明一套格式;如果将来出现现成的 ACP 移动端客户端,也能直接兼容。
- **客户端:** 首选 PWA(普通网页即可,不需要 wasm 和 GPU 渲染),再用 Capacitor 包装成 App,或者做 SwiftUI、Compose 原生客户端。

## 5. 坑

1. **线程只有被 `AgentPanel` 加载后才"活着"。** 运行中的线程挂在 `AgentPanel.retained_threads` 或当前视图里(`agent_panel.rs:1115`)。远程打开一个没加载的线程要调用 `load_agent_thread`,这会**切换桌面上 agent 面板当前显示的线程**。可以接受这个行为;想避免的话,需要研究 `retained_threads` 能否在后台加载而不切换视图。
2. **宿主必须至少开着一个 Zed 窗口。** `AgentPanel` 属于某个 Workspace,写操作也需要 `Window`。全部窗口关闭后就没法操作。如果有多个窗口或多个 workspace(`crates/workspace/src/multi_workspace.rs:305`),还要决定新线程开在哪个窗口里。
3. **排队发送和桌面草稿会冲突。** 只有 `send` 那条路径带排队逻辑,而它读取的是**桌面上的消息编辑器**。如果用 `set_message` 加 `send`,会覆盖你在桌面上写到一半的草稿。建议:线程空闲时直接走 `send_content`(不经过编辑器);线程忙碌时,在服务端自己排队,等收到 `Stopped` 事件再发送,或者给客户端返回"正在运行"。
4. **流式输出更新非常频繁。** 每个 token 都会触发一次 `EntryUpdated`。需要按 50 到 100 毫秒合并,发送文本增量;但工具调用的内容会被整体替换,不能都按追加处理。
5. **批准选项的语义。** 权限请求有多种数据形态(`PermissionRequestData::LegacyTool` 和通用的 ACP 请求)。"始终允许"类选项会写入设置,必须走 `ThreadView` 的路径,才能和桌面行为一致。
6. **Diff 和终端是活的实体。** 只能发送快照,不能序列化对象引用。逐块审阅改动是工作量最大的部分,MVP 建议只做"查看 diff"加"全部保留或撤销"。
7. **断线重连。** 每个线程都要维护序号;重连时先发全量快照,再继续发增量。手机切到后台时 WebSocket 必然断开。
8. **推送通知是这个方案最有价值的部分,但要额外开发。** 例如"等待批准"和"已完成"的提醒。PWA 可以用 Web Push,原生 App 要接 APNs 或 FCM;自托管的 ntfy 是最省事的办法。
9. **安全。** 在手机上批准工具调用,等同于远程执行代码。必须使用 TLS 和设备 token,只在 Tailscale 内网暴露,并且批准操作要记日志。
10. **维护成本。** `agent_ui` 改得非常频繁,建议把对 `ThreadView` 和 `AgentPanel` 的调用收拢到一个薄薄的适配层里,每次同步上游时集中修复。

## 6. 里程碑与工作量(粗估)

| 阶段 | 内容 | 工作量 |
|---|---|---|
| D-MVP | WebSocket 服务加认证;线程列表和状态;条目以 markdown 形式实时推送;发送纯文本(仅限空闲时);批准、拒绝、取消;PWA 客户端 | 2–3 周 |
| D-1 | 结构化条目(工具调用卡片、diff、终端快照);服务端排队;新建或打开线程;断线重连 | 2–3 周 |
| D-2 | 推送通知、elicitation 表单、切换模型和模式、@文件引用、全部保留或撤销改动 | 2–3 周 |

## 7. 与方案 C 的对比

| | 方案 C(绘制指令流) | 方案 D(Agent API) |
|---|---|---|
| 覆盖范围 | 整个 Zed UI | 只有 agent 面板和线程 |
| MVP 工作量 | 4–6 周 | 2–3 周 |
| 手机体验 | 小屏幕上的桌面 UI,中文输入较弱 | 原生交互、原生输入法、推送通知 |
| 弱网表现 | 一般(帧流) | 好(只传语义数据) |
| 依赖接口的稳定性 | 较稳定(`scene.rs` 3 个月 3 次提交) | 变动频繁(`agent_ui` 3 个月 121 次提交) |
| 客户端 | 需要 wasm 和 GPU 渲染器 | 普通网页或原生 UI |

两者不冲突,可以共用同一套 WebSocket 服务和认证:用 D 处理日常的手机 agent 交互和通知,需要完整操作 Zed 时用 C。
