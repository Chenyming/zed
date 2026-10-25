# Agent Panel 新建/命名/删除行为改造：需求与方案

需求 1 到 3 都可行。它们卡在同一个机制上：Agent Panel 始终保留一个绑定在「上次使用的 agent」上的空草稿 thread。
把它改成「没有 thread 时显示 agent 选择页」，需求 1 和 3 就基本解决了；需求 2 相对独立，改动小。
需求 4 在需求 1 的选择页上加一个开关，主要工作是让 acp-mux 也能托管本机的 agent。

## 背景与需求

| # | 需求 | 现在的行为 | 目标行为 | 难度 |
| --- | --- | --- | --- | --- |
| 1 | 新建 thread 时在 panel 里列出可选 agent，点按钮创建 | `cmd-n`、sidebar「+」和打开面板时，都直接用上次使用的 agent 建草稿 | 显示 agent 列表，用户选定后才创建 | 中 |
| 2 | 名字只来自用户：创建时输入，之后可手动重命名；不由 agent 生成 | 发第一条消息前不能改名；原生 agent 每轮结束都会用 LLM 生成标题，外部 agent 自己报标题，首条消息会成为预览标题，都没有开关 | 创建时可起名；默认不采用任何自动标题，没起名就显示固定占位名；配置项可恢复上游行为 | 低到中 |
| 3 | 删除 thread 后不要自动新建 | 删除当前 thread 后先切到 sidebar 里相邻的 thread，没有就用上次的 agent 建新草稿 | 不自动建草稿，显示 agent 选择页 | 做完 1 后低 |
| 4 | 本机项目新建 thread 时，可选择这个 thread 是否「常驻」（走 acp-mux） | 本机 agent 由 Zed 直接启动，Zed 退出或重启就结束；acp-mux 只用于远程项目 | 选择页加「常驻」开关，常驻的 thread 在 Zed 退出后继续运行，可在网页查看；远程项目保持现状 | 中 |

本文的「thread」指 agent panel 里的会话；需求 3 的「删除 agent」按「删除或归档 thread」理解。

## 现状分析

面板只要处于 `BaseView::Uninitialized`，就会自动用 `selected_agent` 建一个真实的草稿 session。三个需求的痛点都来自这里。

### agent 选择与草稿

- `selected_agent` 就是上次使用的 agent，存在全局 KVP 的 `LastUsedAgent` 里（`agent_panel.rs:187`），新开的 workspace 也会继承。
- `ensure_draft`（`agent_panel.rs:3004`）会立刻调用 `create_agent_thread_with_server` 建真实 session。对外部 agent，这一步就会启动 agent 进程。
- 「草稿」的判定是 `AcpThread::is_draft_thread()`，即 entries 为空（`acp_thread.rs:3830`）。
- 自动建草稿的入口（非测试代码中 `activate_draft` 约 12 处调用）：
    - 面板被激活：`set_active` → `ensure_thread_initialized`（`agent_panel.rs:5112`、`agent_panel.rs:5164`）
    - `NewThread` / `cmd-n` → `activate_new_thread`（`agent_panel.rs:1753`）
    - sidebar 的新建按钮（`sidebar.rs:7172`）
    - `clear_base_view`（`agent_panel.rs:1744`）、关闭终端（`agent_panel.rs:2347`）
    - 删除 thread（`agent_panel.rs:3349`、`sidebar.rs:5313`）
- 工具栏 `+` 菜单（`agent_panel.rs:5905`）**已经**列出 Zed Agent、Terminal 和所有外部 agent，点哪个就用哪个建 thread。
  列表数据来自 `agent_server_store.external_agents()` 和 `AgentRegistryStore`。

### 标题

- 显示优先级：`title_override`（用户重命名，存在 sidebar 元数据库）→ `session_info.title`（正式标题）→ `provisional_title`（预览标题）→ `"New Agent Thread"`。
- 发第一条消息时，以消息首行（最多 200 字符）作预览标题（`thread_view.rs:2066`），不写回 agent、不落库。
- 原生 agent 每轮结束时，只要 `title.is_none()` 就调用 `generate_title`（`thread.rs:3267`）。
  模型取 `thread_summary_model` → `default_fast_model` → `default_model`（`registry.rs:495`），没有开关。
- 空 thread 时，工具栏显示的是 `"New {agent} Thread"` 标签（`agent_panel.rs:6182`），不是标题编辑框，所以发第一条消息前改不了名。
- 手动重命名走 `apply_renamed_title`（`thread_view.rs:2946`）：写 `title_override`，并在支持时调用 `AcpThread::set_title` 回写 agent。
  原生 thread 的 `title` 因此有值，自动生成会自然跳过。
- 草稿的元数据条目在 agent **连接成功后**才写入（`conversation_view.rs:1156` 发出 `RootThreadUpdated`，`session_id` 为空）。
  之前调用 `set_title_override` 会因为找不到条目而直接返回（`thread_metadata_store.rs:709`）。

### 删除

- 面板侧 `remove_thread_internal` 已有 `activate_draft_after_remove` 参数，也有现成的 `remove_thread_without_activating_draft`（`agent_panel.rs:3315`）。
- sidebar 归档或删除当前 thread 时，先激活相邻 thread，没有才调用 `activate_draft`（`sidebar.rs:5300` 附近）。
- 单纯不建草稿会让面板停在 `Uninitialized`：一片空白（`agent_panel.rs:6665`），而且下次面板被激活又会自动建草稿。

## 方案总览

核心改动是一个新面板状态 `AgentPicker` 和一个统一入口 `activate_empty_state`，再加两个配置项。需求 1 和 3 靠它们实现，需求 2 独立实现。

```text
打开面板 ─────────────┐
cmd-n / NewThread ────┤
sidebar 新建按钮 ─────┼──> activate_empty_state ──> 新建方式?
删除当前 thread ──────┤     （新增的统一入口）        │
clear / 关闭终端 ─────┘                               ├─ 默认 ──> 沿用上次 agent，立即建草稿（现有行为）
                                                      │
                                                      └─ 选择页 ──> AgentPicker 选择页（列出 agent，可填名字）
                                                                         │ 点选 agent
                                                                         v
                                                                    草稿 thread（绑定所选 agent）
```

五个「没有 thread 可显示」的入口都汇到 `activate_empty_state`；默认模式保持现有行为，选择页模式下要等用户点选 agent 才建草稿。

配置项示例（名字待定）：

```json
{
  "agent": {
    "new_thread_agent_selection": "last_used", // 或 "picker"
    "auto_generate_thread_title": true
  }
}
```

## 需求 1：agent 选择页

新增一个面板状态 `BaseView::AgentPicker`，用配置项控制「没有 thread 可显示时」是建草稿还是显示选择页。本 fork 默认显示选择页；现有测试在初始化时切回上游行为，因此不受影响。

### 改动点

1. **抽出 agent 列表**：把 `+` 菜单里构建列表的代码（`agent_panel.rs:5987` 起，含显示名、图标、排序、collab 禁用）抽成一个函数，菜单和选择页共用。
2. **新增状态与渲染**：`BaseView::AgentPicker` 和 `VisibleSurface::AgentPicker`。渲染一个列表：Zed Agent、Terminal（支持时）、外部 agent、Add More Agents。
   点击后调用 `set_selected_agent_and_persist` + `activate_new_thread`。
3. **统一入口**：新增 `activate_empty_state(focus, source, window, cx)`，按配置调用 `activate_draft` 或进入 `AgentPicker`。把「没东西可显示」含义的调用点换成它：
    - `ensure_thread_initialized`（面板激活）
    - `new_thread` / `cmd-n`
    - sidebar 新建按钮（`sidebar.rs:7172`），改调一个公开的 `show_agent_picker`
    - `clear_base_view`、关闭终端、删除 thread
    - `+` 菜单里的具体 agent 条目是用户显式选择的，**不改**
4. **附属逻辑**：
    - `activation_focus_handle` 返回选择页的焦点，支持上下键 + 回车
    - 工具栏在该状态下显示「New Thread」
    - `active_thread_id` 返回 `None`，sidebar 不高亮任何条目
    - 序列化时不保存该状态，重启后照常恢复上次活动的 thread
5. **模式影响**：选择页模式下忽略 `last_created_entry_kind`（上次建的是终端就默认建终端），Terminal 作为列表里的一项。collab 项目只启用 Zed Agent。

### 难点

- `agent_panel.rs` 有 1.47 万行，草稿逻辑和这些东西缠在一起：`draft_thread` 槽位、`retained_threads`、草稿 prompt 的 KVP 存储、`restore_new_draft`、sidebar 里草稿条目的显示。
- `activate_draft` 调用点里混着「要一个草稿」和「没东西显示」两种含义，要逐个判断。
- 顺带的好处：不再为用户没选的 agent 提前启动进程，远程场景下也少了无用的 session。

## 需求 2：名字只来自用户输入

已确认的规则（2026-10-21）：

- thread 的名字只来自用户：创建时输入，或之后手动重命名。
- agent 不生成名字。原生 agent 不调用 LLM 生成标题；外部 agent 自己报的标题不采用；第一条消息也不再作为预览标题。
- 创建时可以留空。留空就一直显示固定占位名「New {agent} Thread」，不会自动变成别的名字。
- 之后仍可手动重命名，入口和现在一样。
- 用配置项控制，本 fork 默认关闭自动标题；打开后恢复上游行为。

### 配置项

在 `settings_content/src/agent.rs` 和 `AgentSettings` 里加 `auto_generate_thread_title: bool`（名字待定），写进 `assets/settings/default.json`，**默认 `false`**。
关闭时屏蔽三个自动标题来源；打开时完全是上游现在的行为：

| 来源 | 位置 | 关闭时 |
| --- | --- | --- |
| 原生 agent 用 LLM 生成标题 | `thread.rs:3268` 调用 `generate_title` | 不调用 |
| 外部 agent 通过 `SessionInfoUpdate` 报的标题 | `AcpThread::update_session_info`（`acp_thread.rs:5309`） | 忽略 `title` 字段，其余字段（如更新时间）照常应用 |
| 第一条消息首行作预览标题 | `thread_view.rs:2083` 调用 `set_provisional_title` | 不调用 |

- 手动「重新生成标题」（`regenerate_title_with_callback`）是用户的显式操作，不受开关影响，结果作为用户起的名保存。
  关闭时也可以考虑直接隐藏这个菜单项（待确认）。
- 屏蔽外部 agent 标题的判断放在 `AcpThread` 里而不是 `agent_servers` 里，原生 agent 和外部 agent 走同一个入口，规则只写一处。
  `acp_thread` 读取 `AgentSettings` 需要确认依赖关系；不方便的话，由 `AcpThread` 持有一个布尔值，在创建时从设置读入。
- 默认值改为 `false` 会影响上游断言「自动生成了标题」的测试。这些测试显式打开配置项即可，不改断言。

### 创建时起名

- **方案 A（推荐，改动最小）**：`EmptyThread` 模式下工具栏不再显示 `"New {agent} Thread"` 标签（`agent_panel.rs:6182`），改用已有的 `title_editor`，以该文案作 placeholder。编辑后照常走 `apply_renamed_title`。
- **方案 B**：在需求 1 的选择页顶部加一个名字输入框，选定 agent 后把名字带给新草稿。A 和 B 可以并存。
- 两种方式最后都走 `apply_renamed_title`：写 `title_override`，并在支持时调用 `AcpThread::set_title` 回写 agent。

### 需要处理的坑

- **连接前的名字会丢（只影响方案 B）**：草稿的元数据条目要到 agent 连接成功后才写入。外部 agent 启动可能要几秒，这期间的 `set_title_override` 会被丢弃。
  方案 A 不受影响：标题框属于 `ThreadView`，连接成功后才存在，而连接成功时元数据已同步写入；连接期间工具栏仍显示占位名标签。
  方案 B 的对策：`ConversationView` 暂存 `pending_title`，在 `conversation_view.rs:1156` 连接成功处补写。
- **切换 agent 会删掉已命名的空草稿**：`ensure_draft` 和 `activate_new_thread` 在 agent 不匹配、草稿又没内容时，会删除旧草稿的元数据。
  「有 `title_override`」要算作有内容，即把它加进 `draft_has_content` 和 `ensure_draft` 的判断。
  `try_make_empty_draft_ephemeral`（清空输入框后把草稿变回临时草稿）同样要排除已命名的草稿。
- **占位名用 placeholder**：没有标题时标题框保持为空，用 placeholder 显示「New {agent} Thread」，起名时不必先删掉默认文字。
  失焦时若被清空，恢复为当前标题（没有则保持为空），不再写回 `"New Agent Thread"`。
- **不会互相覆盖**：开关关闭时只有用户输入会写标题，不存在覆盖问题。开关打开时沿用上游的优先级：`title_override` 高于 agent 的标题。
- **历史列表里的标题**：外部 agent 的线程历史（`AcpSessionList`）显示的是 agent 在 `session/list` 里报的标题，和面板标题是两条路。
  本地的 sidebar 以元数据库的 `title_override` 为准，不受影响；agent 侧历史列表是否也要过滤，待确认。

### 与 acp-mux 的衔接（之后做）

acp-mux 的网页目前只显示 agent 报的标题，Zed 里起的名字和重命名都不会同步过去：外部 agent 的连接没有实现 `set_title`（`connection.rs:261` 的默认实现），名字从不离开 Mac。
本需求完成后再做，规则照搬这里：

1. Zed 端：agent 由 acp-mux 托管时（`initialize` 响应带 `zed.dev/acpMux`），`AcpConnection::set_title` 返回一个实现，发送扩展请求 `_zed.dev/acpMux/set_title {sessionId, title}`。
   创建时起名、重命名都经过 `AcpThread::set_title`，不需要另找入口。
2. acp-mux 端：保存为会话的用户标题，记进快照，向其他观察者广播 `session_info_update`；不转发给 agent（ACP v1 没有对应方法）。
3. Zed 的 `auto_generate_thread_title` 管不到 daemon：daemon 目前在 `session.rs` 的 `record_update` 里直接采用 agent 报的标题，
   用于 `acp-mux list`、网页的列表和标题栏、ntfy 通知标题（`display_title()`），以及返回给 Zed 的 `session/list`。
   daemon 自己加同名配置项，默认 `false`。关闭时不保存 agent 报的 `title`，转发和写快照前去掉该字段；各处只用用户标题，没有就显示占位名。
   不按连接传 Zed 的配置：同一会话可能同时挂着 Zed 和网页，标题由 daemon 统一决定，各端才一致。

## 需求 3：删除后不自动新建

有了需求 1 的 `activate_empty_state`，这里只是替换两处调用；单做这一条没有意义，因为面板停在 `Uninitialized` 会一片空白，下次激活又会建草稿。

### 改动点

1. `remove_thread_internal`（`agent_panel.rs:3349`）：`activate_draft` 改为 `activate_empty_state`。
2. sidebar 归档/删除后的兜底（`sidebar.rs:5313`）：同上，改调面板的公开方法。
3. 删除的正好是空草稿时，已有逻辑会清掉 `draft_thread` 槽位，之后同样进入选择页。
4. 「先切到相邻 thread」的行为暂时保留，它不属于新建（见待确认问题）。

删除对应的外部 agent 进程和 session 如何回收，不在本需求范围内，行为不变。

## 需求 4：本机 thread 可选常驻（acp-mux）

已确认的规则（2026-10-23）：

- 只针对本机项目。远程项目保持现状：按远程主机上 `acp_mux.json` 的配置，所列 agent 全部常驻，这部分不改。
- 在选择页上加一个「常驻」开关，按 thread 选择。开关关闭时和现在一样，由 Zed 直接启动 agent。
- 已经创建的 thread 不能切换方式。

### 现状

- acp-mux 只在 remote server 里接入：`headless_project.rs:1546` 给 `AgentServerStore` 设置 `command_wrapper`。
  这个 wrapper 只在远程取 agent 命令的处理函数里调用（`agent_server_store.rs:708`）。
- 本机的外部 agent 在 `custom.rs:262-271` 取到命令后由 Zed 直接启动，没有经过 wrapper。
- 连接按 agent 缓存（`agent_connection_store.rs:71`），一个 agent 一个进程，进程里跑多个 session（thread）。
  走不走 mux 是在启动进程时决定的，所以按 thread 选择意味着同一个 agent 要有两个连接、两个进程。
- daemon 已经支持 macOS：取对端 uid 有 `getpeereid` 分支，用 `setsid` 脱离 Zed，`acp_mux` 的测试在 Mac 上通过。
  socket 路径（`~/Library/Application Support/Zed/server_state/acp-mux/daemon.sock`）76 字节，在 macOS 104 字节的限制内。
- 本机的 `zed` 程序没有 `acp-mux` 子命令。`wrap_command` 用 `current_exe()` 拼命令，本机就是 `zed` 程序。
  `crates/zed/src/main.rs` 里已有同类写法：`sandbox::run_sandbox_launcher_if_invoked()`、`--askpass`、`--crash-handler`，都在初始化界面之前处理并退出。
- mux 在 `initialize` 时关掉了 agent 的文件读写和终端能力，常驻 thread 因此没有改动审阅和内嵌终端（远程也是这样）。
- 应用名在各渠道都是 "Zed"，Stable、Preview 和自己编译的 Zed 共用一个 daemon 和一份配置。

### 方案

本机项目为每个可常驻的 agent 增加一个「常驻版本」，在 Zed 内部当作另一个 agent；选择页用开关在两个版本之间选，列表里仍是一行一个 agent。

1. **`zed acp-mux` 子命令**：在 `main.rs` 参数解析之前识别 `acp-mux`，交给 `acp_mux::run` 后退出，不初始化 GPUI，也不会出现 Dock 图标。
   daemon 的版本跟着 Zed 走；管理命令为 `/Applications/Zed.app/Contents/MacOS/zed acp-mux list|kill|restart`。
2. **派生的常驻 agent**：本机 `AgentServerStore` 读 `~/.config/zed/acp_mux.json`，为 `enabled` 且列在 `agents` 里的外部 agent 各注册一个派生条目。
    - id 由原 id 加固定后缀构成，原 id 和派生 id 的换算集中在一处。
    - 显示名、图标沿用原 agent。
    - 命令是原 agent 的命令经 `wrap_command` 包装后的结果，接在 `custom.rs` 取命令之后。
    - 设置映射回原 agent：`default_config_options`、默认模式和模型、环境变量都不用配两遍。
    - `+` 菜单和选择页的 agent 列表（`external_agent_entries`）不列出派生条目。
    - 本机的 `enabled`、`agents` 含义是「哪些 agent 提供常驻开关」，不会强制所有 thread 常驻；这一点和远程不同，文档里要写清楚。
      `gateway`、`notify`、`auto_approve`、`auto_generate_thread_title` 的含义和远程一样，由 daemon 读取。
3. **选择页开关**：名字输入框下方加「常驻（acp-mux）」开关，旁边用一行说明代价：关闭 Zed 后继续运行、可在网页查看、不支持改动审阅。
    - 选中的条目没有常驻版本时（Zed Agent、Terminal、未配置的外部 agent），开关不可用。
    - 开关状态记住上次的选择，存在全局 KVP 里。
    - 确认时，开关打开就用派生 agent 新建 thread，否则用原 agent。
4. **常驻标记**：thread 的 agent 是派生 agent 时，工具栏和侧边栏条目显示一个小图标，提示文字为「常驻（acp-mux）」，一眼能看出哪些 thread 在 Zed 退出后还在运行。
5. **历史去重**：claude-acp、codex-acp 的历史来自 `~/.claude`、`~/.codex` 等目录，直接启动的版本也会列出常驻版本创建的会话。
   从直接版本打开会让两个进程同时加载同一会话。对策：直接版本的历史列表里，过滤掉元数据库中属于派生 agent 的 `session_id`。
   网页端只能使用已有会话、不能新建，常驻会话都由 Zed 创建，所以元数据库里都有记录。

打开旧 thread 时，元数据里的 `agent_id` 决定走哪个版本，不需要额外字段。

### 限制

| 限制 | 说明 |
| --- | --- |
| 常驻 thread 没有改动审阅和内嵌终端 | mux 关掉了文件读写和终端能力，agent 直接写盘。需要审阅时用直接版本 |
| Mac 休眠 | 休眠时进程被冻结、网络断开，正在运行的轮次可能失败，手机也连不上。需要用户自己设置电源选项或 `caffeinate -s` |
| 两个进程 | 同一个 agent 两种方式都在用时，会跑两个进程。常驻那个在所有 thread 关闭后仍在，可用 `zed acp-mux kill --agent` 结束 |
| 防火墙弹窗 | 打开网关时，自己编译的未签名 `zed` 在每次重新编译后第一次监听端口时可能弹窗。建议只监听 127.0.0.1，由 Tailscale 或 frp 转发 |
| 各渠道共用 daemon | daemon 由先启动它的那个 Zed 程序拉起；协议版本不一致时提示执行 `acp-mux restart` |

### 以后可以做

- **转为常驻**：空闲时先在原进程关闭会话，再由常驻版本加载，把已有 thread 转成常驻。
- **Zed 连着时保留改动审阅**：继续向 agent 声明文件读写能力；会话有 Zed 连着时把请求转给 Zed，没有时由 daemon 直接读写磁盘。远程也能受益。

## 风险、测试与实施顺序

### 风险

| 风险 | 影响 | 对策 |
| --- | --- | --- |
| 上游频繁改 `agent_panel.rs` | 本 fork（`ymzed`）合并时冲突 | 新逻辑集中在新函数里，调用点只做一行替换 |
| `activate_draft` 调用点含义判断错 | 该建草稿的地方出了选择页，或反过来 | 逐个审调用点，每个补一个选择页模式的测试 |
| 焦点丢失 | 选择页下快捷键无响应 | `activation_focus_handle` 返回选择页的 focus handle |
| 连接前起的名字丢失 | 外部 agent 启动期间改名无效 | `pending_title` 在连接成功后补写 |
| 已命名的空草稿被清理 | 切换 agent 后名字消失 | `title_override` 算作草稿有内容 |
| 派生 agent 漏掉某项设置映射 | 常驻版本的默认模型、模式或环境变量和原 agent 不一致 | 原 id 和派生 id 的换算集中在一处，取设置的地方统一先换算 |
| 同一会话被两个进程加载 | 历史互相覆盖或报错 | 直接版本的历史列表过滤掉属于派生 agent 的会话 |
| 改动影响远程 | 远程「按配置全部常驻」的行为被改变 | 派生 agent 只在本机项目注册；远程的 `command_wrapper` 路径不动 |

### 测试

- 配置项默认保持现有行为，`agent_panel.rs` 和 `sidebar_tests.rs` 里大量「新建即有草稿」的测试无需改。
- 新增选择页模式的测试：打开面板、`NewThread`、sidebar 新建、删除当前 thread、选择 agent 后生成草稿。
- 新增命名测试：连接前起名、开关关闭时首行落库、切换 agent 不丢名。
- 需求 4：开关在无常驻版本时不可用；开关打开时新建的 thread 使用派生 agent，命令经过 `acp-mux attach` 包装；
  派生 agent 沿用原 agent 的设置；直接版本的历史列表不含常驻会话；远程项目不注册派生 agent。
- 所有改动跑 `./script/clippy`。

### 待确认问题

- [x] 删除当前 thread 后，是否保留「先切到相邻 thread」：保留。
- [x] 需求 3 是否只在选择页模式下生效：是，不单独加开关。
- [x] 起名入口：方案 A 和 B 都要。
- [x] 两个配置项：`auto_generate_thread_title`（默认 `false`）；`new_thread_agent`，取值 `"ask"`（本 fork 默认，显示选择页）或 `"last_used"`（上游行为）。测试初始化用 `test_support::start_new_threads_with_last_used_agent` 切回上游行为。
- [x] 没起名时显示什么：固定占位名「New {agent} Thread」，不自动改名。
- [x] 创建后是否允许重命名：允许。
- [x] 不由 agent 生成标题是写死还是配置项：配置项，本 fork 默认关闭。
- [x] 开关关闭时隐藏「重新生成标题」菜单项：隐藏。
- [x] 外部 agent 的历史列表（`session/list` 的标题）：Zed 端暂不过滤，在 acp-mux 那步由 daemon 处理。
- [x] 选择页需要键盘导航。
- [x] 需求 4 的入口：选择页上的「常驻」开关，不在列表里多出常驻行。
- [x] 需求 4 的范围：只改本机项目；远程保持按配置全部常驻，不修改。
- [ ] 「常驻」开关的初始值：建议关闭，之后记住上次的选择。
- [ ] 常驻标记用哪个图标。

### 实施顺序

1. 需求 2 的配置项：屏蔽三个自动标题来源，默认关闭。独立、改动最小，可以先合入。（已完成）
2. 需求 2 的方案 A：空 thread 显示标题框，并处理草稿清理的坑。（已完成；`pending_title` 移到第 4 步随方案 B 做）
3. 抽出 agent 列表函数，`+` 菜单改用它（行为不变）。
4. 新增 `AgentPicker` 状态、配置项和 `activate_empty_state`，替换入口调用点。（已完成：选择页在 `agent_picker.rs`；入口为面板激活、`cmd-n`、sidebar 新建、`clear_base_view`、关闭终端；起名在连接后通过 `rename_when_connected` 写入，终端用 `custom_title`）
5. 需求 3：替换删除路径上的两处调用。（已完成；归档走 `clear_base_view`，第 4 步已改）
6. 补测试。（已随各步完成）
7. acp-mux 命名（见需求 2 的「与 acp-mux 的衔接」）（已完成：daemon 配置 `auto_generate_thread_title`，Zed 的 v1 连接在 agent 由 acp-mux 托管时实现 `set_title`）。
8. 需求 4：`zed acp-mux` 子命令。完成后可以先手动验证 daemon 在 Mac 上的启动、`list` 和网关。（约 0.5 天）
9. 需求 4：本机 `AgentServerStore` 注册派生的常驻 agent，命令包装和设置映射。（约 1 天）
10. 需求 4：选择页「常驻」开关、工具栏和侧边栏的常驻标记。（0.5～1 天）
11. 需求 4：历史去重、测试，以及在 `acp-mux.zh-CN.md` 补「本机使用」一节。（约 0.5～1 天）
