---
title: "Zed 架构与技术栈速查"
description: "Zed 客户端、编辑器内核、扩展、远程开发和协作服务的源码导览。"
---

# Zed 架构与技术栈速查

本文基于 Zed `v1.14.2`（commit
`02abf5b08fa12c1c20a155ae3f796ef4c6c1a01e`）整理，用于后续阅读和分析
源码。它是仓库实现的索引，不是上游正式的完整架构规范。

Zed 没有一份集中的 `Architecture.md`。架构信息分散在贡献指南、GPUI
文档、开发术语表、远程开发文档以及各 crate 的 README 和 manifest 中。

## 主要资料入口 {#source-index}

- [贡献指南：Bird's-eye view](../../../CONTRIBUTING.md#birds-eye-view-of-zed)：
  官方核心 crate 地图。
- [GPUI README](../../../crates/gpui/README.md)：UI、状态管理和渲染模型。
- [开发术语表](./glossary.md)：`App`、`Entity`、`Workspace`、`Buffer`
  等概念。
- [远程开发](../remote-development.md)：本地 UI 和远程无头服务的边界。
- [协作服务 README](../../../crates/collab/README.md)：WebSocket、数据库和
  部署信息。
- [根 Cargo.toml](../../../Cargo.toml)：Cargo workspace 和完整依赖清单。
- [Rust 工具链](../../../rust-toolchain.toml)：编译器版本与构建目标。

> **Note:** `docs/src/development/glossary.md` 自身标明它是持续完善中的
> best-effort 列表。阅读架构时应以当前版本源码和 manifest 为最终依据。

## 总体架构 {#overall-architecture}

Zed 不是 Electron 或浏览器前端。它是以 Rust 和自研 GPUI 为核心的原生
桌面应用。仓库使用大型 Cargo workspace，把 UI、编辑器、语言支持、远程
开发、协作和 AI 等功能拆成大量职责较小的 crate。

```text
                         Zed Desktop
┌────────────────────────────────────────────────────────────┐
│ crates/zed：程序入口、服务初始化、功能装配                  │
│                                                            │
│ Workspace / Pane / Panel                                   │
│ ├── GPUI + UI：状态管理、窗口、组件、GPU 渲染              │
│ ├── Editor                                                 │
│ │   └── Buffer / Rope / SumTree / 协同编辑操作             │
│ ├── Project / Worktree                                     │
│ │   ├── 文件系统、搜索、Git                                │
│ │   ├── Tree-sitter 语法分析                               │
│ │   ├── Language Server Protocol                           │
│ │   ├── Debug Adapter Protocol                             │
│ │   ├── Terminal / Tasks                                   │
│ │   └── Local / Remote abstraction                         │
│ ├── Extension Host：WebAssembly/WASI 扩展                  │
│ └── Agent / Language Models / MCP / ACP                    │
└────────────────────────────────────────────────────────────┘
          │ WebSocket + Protobuf         │ SSH
          ▼                              ▼
┌───────────────────────┐      ┌──────────────────────────┐
│ Zed Collab Server     │      │ Remote Zed Headless      │
│ Axum + Tokio          │      │ 文件、LSP、终端、任务    │
│ PostgreSQL            │      │ 在远程机器执行           │
│ LiveKit/WebRTC        │      └──────────────────────────┘
└───────────────────────┘
```

## 客户端分层 {#client-layers}

### 程序装配层 {#application-composition}

[`crates/zed`](../../../crates/zed) 是桌面程序入口。它初始化 GPUI、文件
系统、HTTP 客户端、本地数据库、配置、主题、语言注册表和各个面板，并把
编辑器、项目、协作、扩展、AI、终端、调试器等 crate 组合成最终应用。

默认 Cargo workspace member 是 `crates/zed`。功能代码本身继续下沉到
独立 crate，入口 crate 主要承担初始化和装配职责。

### UI 和状态层 {#ui-and-state}

[`gpui`](../../../crates/gpui) 是 Zed 自研的 Rust GPU UI 框架。它采用即时
模式和保留模式混合设计：

- `App` 保存应用级状态和所有实体，主要存在于 UI 线程。
- `Entity<T>` 是由 GPUI 管理的强类型状态引用。
- `View` 实现 `Render`，每帧构造声明式 Element 树。
- `Element` 负责布局、命中测试和底层绘制，适合编辑器和大型列表等需要
  精细控制的场景。
- `Action`、`Event`、`Subscription` 连接输入、事件和状态更新。
- `ForegroundExecutor` 调度主线程任务，`BackgroundExecutor` 调度后台
  工作。

[`ui`](../../../crates/ui) 在 GPUI 之上提供共享组件和交互模式。
[`workspace`](../../../crates/workspace) 是窗口内容的根，组织 Pane、Dock、
Panel 和 Project，并负责会话状态持久化。

平台后端由 [`gpui_platform`](../../../crates/gpui_platform) 选择：

| 平台          | 窗口与文本         | 渲染路径            |
| ------------- | ------------------ | ------------------- |
| macOS         | AppKit、CoreText   | Metal               |
| Linux/FreeBSD | Wayland 或 X11     | `wgpu`              |
| Windows       | Win32、DirectWrite | GPUI Windows 后端   |
| Web 实验目标  | Web APIs           | WebAssembly、`wgpu` |

### 编辑器和文本层 {#editor-and-text}

主要对象链路如下：

```text
Workspace → Project → Worktree → Buffer → Editor
```

- [`workspace`](../../../crates/workspace) 管理窗口和会话。
- [`project`](../../../crates/project) 管理 Worktree、文件、语言服务、Git、
  调试器和本地/远程执行边界。
- [`worktree`](../../../crates/worktree) 表示本地或远程文件树。
- [`text`](../../../crates/text) 提供可协同编辑的 `Buffer`。
- [`editor`](../../../crates/editor) 处理展示、光标、选择、输入、补全、
  diagnostics 和 inlay hints。
- [`multi_buffer`](../../../crates/multi_buffer) 把多个 Buffer 的范围组合成
  一个可编辑视图，供搜索结果等功能使用。

文本存储不是普通 `String`：

- [`rope`](../../../crates/rope) 使用 `SumTree<Chunk>` 保存文本，支持大型
  文件和多种坐标维度下的位置查询。
- `BufferSnapshot` 同时保存可见文本、删除文本、fragment tree、插入片段、
  undo map 和版本信息。
- `EditOperation` 和 `UndoOperation` 携带 Lamport timestamp 与 version
  vector。
- [`clock`](../../../crates/clock) 定义 replica ID、Lamport clock 和全局
  version vector，用来排序及追踪分布式编辑事件。

因此，协同编辑能力位于文本模型内部，而不是仅在网络层同步最终文件。

### 语言和开发工具层 {#language-tooling}

[`project`](../../../crates/project) 是开发工具能力的主要协调层：

| 能力           | 主要实现                                          |
| -------------- | ------------------------------------------------- |
| 语法解析与高亮 | Tree-sitter、query `.scm` 文件                    |
| 代码智能       | Language Server Protocol，`language`、`lsp` crate |
| 调试           | Debug Adapter Protocol，`dap`、`debugger_*` crate |
| 终端           | Zed fork 的 `alacritty_terminal`、VTE、PTY        |
| Git            | `git`、`git_ui`、`git_hosting_providers` crate    |
| 格式化         | 外部 formatter、Prettier 等适配层                 |
| Notebook/REPL  | Jupyter protocol 和 WebSocket                     |
| 搜索           | regex、fuzzy、Nucleo、自研树结构                  |

Tree-sitter 负责本地增量语法理解，language server 负责语义能力。两者不是
替代关系。`editor` 展示结果，`project` 管理 language server 生命周期和
请求路由。

## 扩展架构 {#extension-architecture}

Zed 扩展可以提供语言、language server、debugger、主题、图标、snippet 和
MCP server。

需要程序逻辑的扩展使用 Rust 编写，编译为 `wasm32-wasip2` WebAssembly
组件。客户端通过 [`extension_host`](../../../crates/extension_host) 使用
Wasmtime/WASI 加载扩展，并通过
[`extension_api`](../../../crates/extension_api) 暴露受控宿主能力。

大部分纯主题或语法配置扩展不需要 Rust 代码。内置测试扩展示例位于
[`extensions`](../../../extensions)。

## 协作架构 {#collaboration-architecture}

[`collab`](../../../crates/collab) 是本仓库中的 Zed 协作后端。客户端认证后
通过 WebSocket 连接该服务。

主要技术边界：

- [`proto`](../../../crates/proto) 定义客户端与服务端共享的消息协议，使用
  Prost/Protocol Buffers。
- [`rpc`](../../../crates/rpc) 负责 WebSocket 消息传输、压缩、连接和相关
  通信逻辑。
- 服务端使用 Tokio、Axum 和 Tower。
- 持久化使用 PostgreSQL、SeaORM 和 SQLx。
- 实时通话使用 LiveKit/WebRTC。
- manifest 中还包含 S3、Kinesis 和 Prometheus 相关依赖。
- 仓库文档记录的生产部署目标是 DigitalOcean Kubernetes。

登录站点 `zed.dev` 是独立仓库并部署在 Vercel，因此当前仓库不能完整呈现
账号、Web 站点和所有云端基础设施设计。

## 远程开发架构 {#remote-development-architecture}

远程开发和多人云协作是两条不同链路。

远程开发时：

- 本地机器运行窗口、GPUI、Editor UI、Tree-sitter、语言模型交互，并保存
  未写入磁盘的修改和最近项目状态。
- 远程机器运行 headless Zed server，持有源码并运行 language server、
  task 和 terminal。
- 两端通过系统 `ssh` 进程连接，不经 Zed 的协作云中转。
- `Project`、`Worktree` 及多种 `*Store` 抽象隐藏本地与远程实现差异。

相关入口包括 [`remote`](../../../crates/remote)、
[`remote_connection`](../../../crates/remote_connection) 和
[`remote_server`](../../../crates/remote_server)。

## AI 和 Agent 架构 {#ai-and-agent}

AI 能力不是单一 provider 实现，而是分层组织：

- `language_model_core`、`language_model` 和 `language_models` 定义统一模型
  接口与注册机制。
- `anthropic`、`open_ai`、`google_ai`、`bedrock`、`ollama`、`lmstudio` 等
  crate 实现具体 provider。
- `agent` 管理对话、上下文、工具调用、编辑和命令执行。
- `agent_ui` 提供 Agent Panel。
- `context_server` 和相关模块支持 Model Context Protocol（MCP）。
- `acp_thread`、`acp_tools` 和 `agent-client-protocol` 支持 Agent Client
  Protocol（ACP）集成。
- `sandbox` 为 Agent 命令执行提供平台相关隔离能力。

## Agent Panel 独立窗口可行性 {#agent-panel-separate-window}

### 结论 {#agent-panel-separate-window-conclusion}

GPUI 支持创建多个原生窗口，因此把 Agent Panel 放入独立窗口在技术上可行。
但当前的 `AgentPanel` 不是可以直接从 Dock 中取出并挂到另一个窗口的纯视图。
它同时承担了以下职责：

- Workspace Dock 中的 `Panel`。
- Workspace 级 Agent 状态、当前线程和终端的所有者。
- Agent、ACP、MCP 连接及会话相关对象的持有者。
- `ConversationView`、消息编辑器和终端视图等窗口绑定 UI 的所有者。
- Agent 与来源 Workspace、Project、Pane 之间的操作路由。
- 焦点、可见性、通知和状态持久化。

因此，独立窗口应通过“在目标窗口中重新创建窗口专属 UI，并共享或迁移业务状态”
实现，不应把同一个正在运行的 `Entity<AgentPanel>` 同时渲染在两个窗口，也不应
直接把该 Entity 从一个窗口迁移到另一个窗口。

```text
推荐边界

AgentWindowManager
├── WorkspaceId → AgentWindow
├── 创建、激活和关闭窗口
└── 追踪来源 MultiWorkspace

AgentWindow
├── 窗口专属 Agent UI、焦点和订阅
├── WeakEntity<Workspace>
├── Entity<Project>
└── 来源 WindowHandle<MultiWorkspace>

EditorWindowRouter
├── 打开文件和 Diff
├── Reveal in Project Panel
├── 操作 active Pane
└── 创建 Worktree Workspace
```

### 已有的多窗口基础 {#agent-panel-existing-window-support}

GPUI 的 `App::open_window` 和 `WindowOptions` 可以创建普通、浮动、弹出和对话框
窗口。长期使用的 Agent 窗口应采用普通原生窗口。Settings UI 已经提供了可参考的
实现：它创建独立的 `SettingsWindow`，并保存来源
`WindowHandle<MultiWorkspace>`，需要操作项目时再回到来源窗口。参见
[`settings_ui`](../../../crates/settings_ui/src/settings_ui.rs) 中的
`open_settings_editor_with` 和 `SettingsWindow`。

Zed 也已有跨窗口查找及激活 Workspace 的模式。Agent Sidebar 会遍历
`App::windows()`，查找目标 `MultiWorkspace`，然后通过 typed
`WindowHandle<MultiWorkspace>::update` 在正确的窗口上下文中激活 Workspace 和
Agent 线程。参见 [`sidebar`](../../../crates/sidebar/src/sidebar.rs) 中的
`find_workspace_across_windows` 和 `activate_thread_in_other_window`。

这些机制证明底层窗口能力是充足的。当前缺少的是通用的 Panel detach/re-dock
协议；Dock Panel 目前只能在同一 Workspace 的左侧、右侧和底部之间移动。

### 当前实现的关键约束 {#agent-panel-separate-window-constraints}

#### Agent Panel 依赖 Workspace 窗口上下文

[`AgentPanel`](../../../crates/agent_ui/src/agent_panel.rs) 直接持有
`WeakEntity<Workspace>`、`WorkspaceId`、`Entity<Project>`、
`AgentConnectionStore`、`ContextServerRegistry`、会话视图和终端视图。
`AgentPanel::load` 通过 `Workspace::update_in` 在 Workspace 所属窗口中创建 Panel
及其子 UI。这些子 UI 使用 `observe_in`、`subscribe_in`、`spawn_in`、
`update_in` 和 `FocusHandle` 等窗口相关机制。

因此，同一个完整 Agent UI Entity 不适合同时归属于 Dock 窗口和独立窗口。Zed
移动 Project 到新窗口时也采用“序列化、关闭、在目标窗口重新创建”的方式，以避免
异步任务继续使用错误的窗口。参见
[`MultiWorkspace::open_project_group_in_new_window`](../../../crates/workspace/src/multi_workspace.rs)。

#### `workspace.panel::<AgentPanel>()` 是现有发现协议

[`Workspace::panel`](../../../crates/workspace/src/workspace.rs) 只搜索三个 Dock。
Agent actions、Sidebar、ACP Tools、草稿存储、补全上下文和 URL handler 等代码都
通过 `workspace.panel::<AgentPanel>()` 获取 Agent Panel。如果 Panel 从 Dock 中
移除，这些入口将无法发现独立窗口中的 Agent。

独立窗口实现需要增加与视觉容器无关的 Agent host 或注册表，例如按
`WorkspaceId` 查找 Agent controller 和 Agent window，而不是以 Panel 是否存在于
Dock 作为 Agent 是否存在的判断依据。

#### Agent 操作必须路由回编辑器窗口

Agent 中的文件链接、Diff、规则文件、Project Panel reveal、active Pane、当前选择
和 Worktree 创建等操作，目标仍然是来源编辑器 Workspace。独立窗口中的事件回调
收到的是 Agent 窗口的 `&mut Window`，不能将它传给属于另一个
`MultiWorkspace` 的 Workspace API。

这些操作必须通过来源 `WindowHandle<MultiWorkspace>` 投递，在来源窗口的
`update` 回调中获得正确的 `&mut Window` 后再更新 Workspace。否则可能出现焦点树、
Editor、Pane 或窗口级订阅绑定到错误窗口的问题。

#### 通知和可见性基于 Dock

当前 `AgentPanel::is_visible` 通过检查 Panel 是否为某个 Dock 的
`visible_panel()` 判断可见性。线程和终端通知也会检查当前窗口是否为
`MultiWorkspace`、对应 Workspace 是否 active，以及 Agent Panel 是否在 Dock 中
可见。

独立窗口需要把可见性改为检查 Agent 窗口是否存在、是否 active，以及目标线程或
终端是否正在窗口中显示。通知点击后应激活 Agent 窗口，而不是重新 reveal Dock
Panel。

#### 部分流程明确假设 Agent 与 Workspace 同窗

`AgentPanelSiblingHost` 保存了创建它的 `AnyWindowHandle`。创建 sibling thread 和
新 Worktree 时，当前实现明确假设来源 Agent Panel 与新 Workspace 位于同一个 OS
窗口。独立窗口会打破该假设，因此需要分别保存 Agent 窗口、来源编辑器窗口和目标
Worktree Workspace 的窗口 handle。

### 实现范围比较 {#agent-panel-separate-window-options}

| 方案                                                 | 可行性 | 改造量 | 说明                                                                                |
| ---------------------------------------------------- | ------ | ------ | ----------------------------------------------------------------------------------- |
| 新建完整 `MultiWorkspace` 窗口并自动打开 Agent Panel | 高     | 低到中 | 可最大程度复用现有行为，但本质上是另一个完整 Zed 项目窗口，不是纯 Agent 窗口。      |
| 每个 Workspace 一个专用 Agent 窗口，替代 Dock Panel  | 高     | 中到高 | 推荐的第一阶段正式方案；保留单个 Agent UI 实例，并增加 Agent host 和跨窗口路由。    |
| Dock 与独立窗口同时显示相同会话                      | 中高   | 高     | 必须进一步拆分会话 Controller 和窗口 Presentation，不能共享现有完整 UI Entity。     |
| 支持拖拽分离和重新停靠                               | 中高   | 高     | 当前没有通用 Panel tear-off 框架，应采用保存状态、销毁旧 UI、在目标容器重建的协议。 |

### 推荐方案 {#agent-panel-separate-window-recommendation}

第一阶段建议采用以下产品语义：

> 一个 Workspace 最多对应一个 Agent 原生窗口；该窗口固定绑定来源 Workspace，
> Agent UI 只存在于独立窗口中，不与 Dock 同时显示；来源 Workspace 关闭时关闭
> 对应 Agent 窗口。

这种语义与当前按 `WorkspaceId` 保存 Agent Panel 状态的方式一致，也能避免 Agent
在用户切换项目标签时意外改变 Project、工作目录或运行中的会话。

建议分阶段实施：

1. 新增 `AgentWindow` 和按 Workspace 管理窗口的 `AgentWindowManager`。
2. 拆分 `AgentPanel::load`，在来源 Workspace 提取依赖，在 Agent 窗口创建 UI。
3. 用容器无关的 Agent host 查找替换 `workspace.panel::<AgentPanel>()` 发现协议。
4. 新增显式 editor-window router，处理文件、Diff、Pane、selection 和 Worktree
   操作。
5. 调整通知、可见性、窗口关闭和窗口 bounds 持久化。
6. 验证 Native Agent、外部 ACP Agent、terminal thread、远程项目和 sibling
   worktree。
7. 如需 Dock/窗口双视图，再把连接、session、active entry 和序列化状态从
   `AgentPanel` 下沉到 Workspace 级 controller，并为每个窗口分别创建
   presentation entity。

最需要防范的风险依次是：跨窗口使用错误的 `Window`、移出 Dock 后现有 Agent
发现协议失效、会话生命周期与 UI 生命周期混合、窗口绑定的焦点和订阅，以及通知和
sibling worktree 流程中的同窗假设。

## 主要技术栈 {#technology-stack}

| 领域         | 技术                                             |
| ------------ | ------------------------------------------------ |
| 主语言       | Rust，Edition 2024                               |
| 固定工具链   | Rust 1.95.0、rustfmt、Clippy、rust-analyzer      |
| UI           | 自研 GPUI                                        |
| GPU          | Metal、`wgpu`                                    |
| 布局和文本   | Taffy、CoreText、DirectWrite、Cosmic Text、Swash |
| 异步         | GPUI Executor、Smol、Tokio、Futures              |
| CPU 并行     | Rayon                                            |
| 文本结构     | Rope、SumTree、Lamport clock、version vector     |
| 语言支持     | Tree-sitter、Language Server Protocol            |
| 调试         | Debug Adapter Protocol                           |
| 扩展         | Rust、WebAssembly、WASI、Wasmtime                |
| HTTP/TLS     | Reqwest、Rustls                                  |
| RPC          | WebSocket、Protocol Buffers、Prost、Zstd         |
| 本地存储     | SQLite、`sqlez`、`db`                            |
| 协作服务     | Rust、Axum、Tokio、Tower                         |
| 服务端数据   | PostgreSQL、SeaORM、SQLx                         |
| 音视频       | LiveKit、WebRTC                                  |
| 配置与序列化 | Serde、JSON、TOML、JSON Schema                   |
| 构建和发布   | Cargo workspace、Nix、Docker、GitHub Actions     |
| 文档         | Markdown、mdBook                                 |

依赖版本以根 [`Cargo.toml`](../../../Cargo.toml) 和 `Cargo.lock` 为准。

## 核心 crate 速查 {#core-crates}

| Crate                                  | 职责                           |
| -------------------------------------- | ------------------------------ |
| `zed`                                  | 应用入口与功能装配             |
| `gpui`、`gpui_platform`                | UI 状态、渲染、平台抽象        |
| `ui`                                   | 通用 UI 组件                   |
| `workspace`                            | 窗口根状态、Pane、Panel、会话  |
| `editor`                               | 编辑器交互和显示层             |
| `text`、`rope`、`sum_tree`、`clock`    | 文本与协同数据结构             |
| `project`、`worktree`                  | 项目、文件树、工具协调         |
| `language`、`lsp`、`grammars`          | 语言、LSP、Tree-sitter         |
| `dap`、`debugger_ui`                   | 调试协议与 UI                  |
| `terminal`、`terminal_view`            | 终端模型与 UI                  |
| `extension_host`、`extension_api`      | WebAssembly 扩展宿主和 API     |
| `remote*`                              | SSH 远程开发和 headless server |
| `proto`、`rpc`、`client`               | 客户端/服务端协议与连接        |
| `collab`、`collab_ui`                  | 协作后端与客户端协作 UI        |
| `db`、`sqlez`                          | 客户端 SQLite 数据层           |
| `agent`、`agent_ui`、`language_model*` | Agent 和模型抽象               |

## 推荐阅读顺序 {#reading-order}

### 理解桌面客户端 {#reading-client}

1. `CONTRIBUTING.md` 的 Bird's-eye view。
2. `crates/gpui/README.md`。
3. `docs/src/development/glossary.md`。
4. `crates/zed/src/main.rs`。
5. `crates/workspace`、`crates/project`、`crates/editor`。

### 理解文本与协同编辑 {#reading-collaborative-text}

1. `crates/clock/src/clock.rs`。
2. `crates/sum_tree/src/sum_tree.rs`。
3. `crates/rope/src/rope.rs`。
4. `crates/text/src/text.rs`。
5. `crates/proto/proto`、`crates/rpc`、`crates/collab`。

### 理解远程开发 {#reading-remote}

1. `docs/src/remote-development.md`。
2. `crates/remote_connection`。
3. `crates/remote`。
4. `crates/remote_server`。
5. `crates/project` 中的 local/remote store 实现。

### 理解扩展和 AI {#reading-extensions-ai}

1. `docs/src/extensions/developing-extensions.md`。
2. `crates/extension_api` 和 `crates/extension_host`。
3. `docs/src/ai/overview.md`。
4. `crates/language_model*`。
5. `crates/agent`、`crates/agent_ui` 和 `crates/context_server`。

## 更新检查清单 {#update-checklist}

当仓库版本升级时，重新检查以下内容：

1. `crates/zed/Cargo.toml` 中的应用版本。
2. `rust-toolchain.toml` 中的 Rust 版本和 target。
3. 根 `Cargo.toml` 中的 workspace member 与关键依赖。
4. `crates/gpui/README.md` 中的平台渲染说明。
5. `crates/collab/Cargo.toml` 和 README 中的服务端依赖与部署说明。
6. `docs/src/remote-development.md` 中的本地/远程职责边界。
7. `crates/extension_api`、`extension_host` 中的 WASI/Wasmtime 版本。
8. 新增或拆分的 `agent*`、`language_model*`、`remote*` crate。
