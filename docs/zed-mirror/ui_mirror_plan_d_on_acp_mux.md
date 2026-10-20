# 方案 D 的 acp-mux 版本:用网页和手机 App 操作远程 agent

> 2026-10-19 的讨论结论。阶段 1、2 和阶段 3 的大部分已实现,实现状态和与本文的差异见第 11 节。
> 相关文档:[方案 D(Zed 内的 Agent 语义层 API)](./ui_mirror_plan_d.md)、[acp-mux 实现方案](./acp_mux_implementation.md)、[acp-mux 使用方法](../src/development/acp-mux.zh-CN.md)

## 1. 结论

- **在 acp-mux 上实现方案 D,复杂度明显低于原方案。** 原方案在 Mac 上的 Zed 里调用 `AgentPanel`、`ThreadView`;新方案让网页和手机 App 作为 acp-mux daemon 的又一个 ACP 客户端,和 Zed 平级。会话状态、断线重放、授权请求的挂起和补发,acp-mux 都已经实现了。
- **MVP 约 2.5–3 周:** 服务端约 1–1.5 周,网页客户端约 1.5 周。
- **主要代价是覆盖范围变窄:** 只能操作 acp-mux 管理的会话,也就是 SSH 远程项目里的外部 agent。Zed 内置 agent 和本地项目的会话不在范围内。
- **接口:** 主通道是 WebSocket 上的 ACP JSON-RPC,另有少量 HTTP JSON 接口处理登录、推送注册等。网页和以后的原生 App 共用同一套接口。
- **前提:** 先用真实的 Zed 和真实的 agent 验证 acp-mux 本身(见 `acp_mux_implementation.md` 第 11.7、11.8 节)。目前只用假 agent 验证过。

## 2. 需求和约束

| 项 | 约定 |
|---|---|
| 客户端 | 先做网页版(PWA),之后扩展手机原生 App(iOS、Android) |
| 会话范围 | **只操作已有的会话**,不在手机上新建会话,也不启动 agent |
| 网络 | 外层由 VPN 或 frp(https)提供访问;网关本身不处理 TLS |
| 终端 | 不需要 |

需要支持的操作:查看会话列表和状态,实时查看消息流,发送文本消息,批准或拒绝工具调用,取消正在执行的轮次,接收"等待授权""已完成"通知。

## 3. 和原方案 D 的对比

### 3.1 原方案 D 的坑,在这里的情况

| 原方案 D 的坑(见 `ui_mirror_plan_d.md` 第 5 节) | 基于 acp-mux |
|---|---|
| 1. 线程要被 `AgentPanel` 加载才"活着",远程打开会切换桌面视图 | 不存在。会话在 daemon 里一直活着,与桌面无关 |
| 2. 宿主必须开着 Zed 窗口 | 不存在。daemon 在 Linux 上常驻,Mac 关机也能操作 |
| 3. 排队发送会覆盖桌面上的草稿 | 不存在。不经过桌面的消息编辑器;daemon 已有忙碌时排队 |
| 4. 流式更新太频繁 | ACP 的 `session/update` 本身就是增量,直接转发,节流可选 |
| 5. 批准选项的语义复杂 | 直接使用 ACP 的权限选项;"始终允许"由 agent 自己处理 |
| 6. diff 和终端是活的实体 | diff 在工具调用内容里(`oldText` / `newText`),可以直接显示;终端不需要 |
| 7. 断线重连要维护快照和序号 | 已实现:`session/load` 重放压缩后的快照、补发授权请求、告知轮次状态 |
| 8. 推送通知要额外开发 | 仍要开发,但 daemon 本来就知道"等待授权"和"轮次结束",加钩子即可 |
| 9. 安全 | 仍然需要,见第 7 节 |
| 10. `agent_ui` 改动频繁 | 大幅降低。只依赖 ACP 协议,不碰 `agent_ui` |

### 3.2 覆盖范围

| | 原方案 D | 基于 acp-mux |
|---|---|---|
| SSH 远程项目里的外部 agent | 支持 | 支持(需要开启 acp-mux) |
| Zed 内置 agent | 支持 | **不支持**(它运行在 Mac 的 Zed 里) |
| 本地项目里的 agent | 支持 | **不支持** |
| 需要 Mac 开着 Zed | 需要 | **不需要** |
| 改动审阅、保留或撤销 | 可以(通过 action log) | 不可以(acp-mux 的限制) |
| 线程元数据 | 项目名、归档状态等 | 只有 cwd、标题、更新时间、agent 名称 |
| 主要维护负担 | 跟随 `agent_ui` 的频繁改动 | 跟随 ACP 协议的版本 |

## 4. 架构

```
 手机 / 浏览器
   │ https + WebSocket(VPN 或 frp 提供 TLS)
   ▼
 acp-mux gateway(remote_server acp-mux serve,只监听 127.0.0.1)
   │  · 静态文件:网页客户端
   │  · HTTP JSON:登录、推送注册、健康检查
   │  · WebSocket:每个连接桥接成 daemon 的一个"观察者" client
   ▼ unix socket
 acp-mux daemon(已有)
   ▼ stdio
 agent 进程(已有)

 桌面 Zed ──ssh──> acp-mux attach ──unix socket──> daemon(已有路径,不变)
```

- gateway 作为 `acp-mux` 的新子命令,随 remote_server 一起分发,不需要单独部署。可以由 daemon 拉起,也可以交给 systemd 管理。
- gateway 不含业务逻辑,只做认证和帧转换:一个 WebSocket 文本帧对应 daemon 连接上的一行 JSON。
- 网页和 WebSocket 同源,不需要处理跨域。

## 5. 接口设计

### 5.1 选型

| 方案 | 结论 |
|---|---|
| **WebSocket + ACP JSON-RPC** | **采用。** 双向、实时;授权请求是 agent 发起、等客户端回答的请求,JSON-RPC 天然支持;直接复用 acp-mux 的路由,和桌面 Zed 共用一套语义 |
| 纯 REST(+ SSE) | 不采用。消息流需要 SSE 或轮询;服务端发起的授权请求要自己设计"推送请求、POST 回答、配对、超时",等于重做 JSON-RPC;每次 ACP 升级都要同步维护两套模型 |
| gRPC | 不采用。浏览器需要 grpc-web 代理,规模不匹配 |
| GraphQL 订阅 | 不采用。偏重,且与 ACP 的消息模型不匹配 |

原生平台都能直接使用 WebSocket(iOS 的 `URLSessionWebSocketTask`、Android 的 OkHttp)。ACP 提供 JSON Schema,可以生成 TypeScript、Swift、Kotlin 的类型。

### 5.2 WebSocket 主通道

地址:`wss://<域名>/api/v1/acp`。连接建立后,客户端以观察者身份连到 daemon,之后收发的都是 ACP JSON-RPC 消息。

| 方法 | 方向 | 说明 |
|---|---|---|
| `initialize` | 客户端 → 服务端 | daemon 直接回答,宣告 `loadSession`、`list`,带 `_meta["zed.dev/acpMux"]` |
| `session/list` | 客户端 → 服务端 | 列出**所有 agent** 的会话;每项的 `_meta["zed.dev/acpMux"]` 带 agent 名称、状态(运行中、等待授权、空闲、已退出)、是否有其他端在观察 |
| `session/load` | 客户端 → 服务端 | 观察一个会话:先重放快照,再告知轮次状态,然后补发等待中的授权请求(与桌面 Zed 相同) |
| `session/close` | 客户端 → 服务端 | 停止观察,不影响 agent |
| `session/prompt` | 客户端 → 服务端 | 发送消息。轮次正在执行时进入 daemon 的队列 |
| `session/cancel` | 客户端 → 服务端 | 取消当前轮次 |
| `session/set_mode`、`session/set_config_option` | 客户端 → 服务端 | 第二阶段 |
| `session/update` | 服务端 → 客户端 | 实时增量 |
| `session/request_permission` | 服务端 → 客户端(请求) | 授权请求,客户端回答所选的选项 |
| `elicitation/create` | 服务端 → 客户端(请求) | 第二阶段 |
| `$/cancel_request` | 服务端 → 客户端 | 授权请求已被其他端回答或被 agent 撤销,客户端移除对应的提示 |
| `_zed.dev/acpMux/turn_state` | 服务端 → 客户端 | 轮次开始(`running`)和结束(`idle` 加停止原因),已在桌面 Zed 中使用 |

观察者连接**不允许** `session/new`、`authenticate` 等需要 agent 进程的方法,daemon 返回 method not found。

### 5.3 HTTP JSON 接口

| 接口 | 用途 |
|---|---|
| `POST /api/v1/login` | 用 token 登录。网页设置 `HttpOnly`、`Secure` 的 cookie;原生 App 取回凭据,之后放在 `Authorization` 头里 |
| `POST /api/v1/logout` | 注销当前凭据 |
| `GET /api/v1/health` | 健康检查,返回 daemon 版本和协议版本 |
| `POST /api/v1/push/register` | 注册推送目标(Web Push 订阅,或 APNs / FCM 设备 token) |
| `GET /` 等 | 网页客户端的静态文件 |

浏览器的 WebSocket API 不能设置请求头,所以网页依赖 cookie;原生 App 使用 `Authorization` 头。gateway 两种方式都支持。

### 5.4 版本

- REST 路径带 `/api/v1/` 前缀。
- 观察者握手沿用 acp-mux 的 `acp_mux` 协议版本号;ACP 本身的版本在 `initialize` 中协商。
- ACP 没有的功能,按 ACP 的约定以 `_zed.dev/acpMux/` 为前缀扩展,与现有的 `turn_state` 一致。

## 6. 服务端改动

### 6.1 观察者握手(约 1 天)

- 新的握手类型,例如 `{"acp_mux": 2, "observe": {}}`,不带 agent 启动命令,不会启动 agent。
- router 中 client 的 agent 改为可选;观察者的 `session/list` 列出所有 agent 的会话,`initialize` 由 daemon 直接回答。
- `session/prompt`、`session/cancel`、授权回答本来就按会话路由到会话所属的 agent,基本不用改。
- 协议版本升为 2。daemon 目前要求版本号完全相同,需要改为同时接受版本 1 的 attach 握手,否则升级期间仍在运行的旧 attach 会被拒绝。

### 6.2 多端同时观察(约 3–5 天)

现在一个会话只有一个 observer,后来的挤掉先来的。手机和桌面同时打开同一个会话时会互相挤掉,所以要改为 observer 集合:

| 事件 | 新的行为 |
|---|---|
| agent 的 `session/update` | 发给所有 observer |
| agent 的授权请求 | 发给所有 observer;**先回答的生效**,其余的 observer 收到 `$/cancel_request` |
| 某一端发送 prompt | daemon 把这条用户消息以 `user_message_chunk` 发给其他 observer,并发送 `turn_state: running`;轮次结束时向它们发送 `idle`。桌面 Zed 已能处理(`AcpThread::attach_external_turn`),不需要改 Zed |
| 某一端取消 | 照常转发给 agent;其他端通过 `idle(cancelled)` 得知结束 |
| `session/load` | 加入 observer 集合,不再挤掉其他端 |
| `session/close` 或断开 | 只把这一端移出集合 |

这是 router 的核心改动,需要补充对应的单元测试和端到端测试。端到端测试可以沿用 `acp_mux_implementation.md` 11.7 节的方式:用驱动程序模拟多个客户端,远端运行脚本化的假 agent。

可以先不做:MVP 沿用"挤掉"的行为也能跑,代价是手机打开某个会话后,桌面 Zed 里的这个线程停止更新,回到电脑前要重新打开线程。**建议做上**,越早改越好。

### 6.3 gateway(约 2–3 天)

- 子命令 `acp-mux serve --listen 127.0.0.1:<端口>`,读取 `acp_mux.json` 中的 gateway 配置。
- WebSocket 使用工作区已有的 `async-tungstenite`。
- 定时发送 ping,避免 frp 或反向代理因空闲断开连接。
- 认证失败、连接、授权回答都记录日志。

### 6.4 推送通知(约 1–2 天)

daemon 在以下事件发生时调用通知钩子:

- 有新的授权请求,并且一段时间内(例如 30 秒)没有任何 observer 回答;
- 轮次结束(`end_turn`、`cancelled`、`max_tokens`、`refusal`、出错)。

实现顺序:

1. 先接 ntfy:向配置的地址发 HTTP 请求。ntfy 有现成的 iOS、Android 和网页客户端,自己的 App 完成前就能使用。
2. 网页客户端完成后接 Web Push(iOS 上需要先把网页添加到主屏幕,iOS 16.4 起支持)。
3. 原生 App 完成后接 APNs / FCM,设备通过 `POST /api/v1/push/register` 注册。

### 6.5 会话持久化(可选,约 3–5 天)

即 `acp_mux_implementation.md` 中 M3 的持久化。daemon 重启后快照会丢失,手机场景更依赖历史,这一项的价值比原先高。

### 6.6 配置示例

```json
{
  "enabled": true,
  "agents": ["claude-acp", "codex-acp"],
  "gateway": {
    "listen": "127.0.0.1:8765",
    "token_file": "~/.config/zed/acp_mux_token"
  },
  "notify": {
    "ntfy_url": "https://ntfy.example.com/zed-agent",
    "permission_delay_seconds": 30
  }
}
```

token 放在单独的文件里(权限 0600),不写进配置文件。

## 7. 安全

在手机上批准工具调用,等于在服务器上远程执行代码。

- **TLS:** 由 VPN 或 frp(https)提供。走 frp 时只允许 https,否则 cookie 和 token 会明文传输。
- **监听地址:** gateway 只监听 `127.0.0.1`,只能经由 frp 或 VPN 访问。
- **认证:** 必须开启,特别是 frp 会把服务暴露到公网。token 足够长且随机;登录接口做失败次数限制;cookie 设置 `HttpOnly`、`Secure`、`SameSite=Strict`。
- **WebSocket 来源检查:** 校验 `Origin`,防止其他网站借用浏览器里的 cookie 连接。
- **审计:** 每次授权回答记录来源(桌面或 gateway、设备)、会话、工具调用和所选选项。

## 8. 客户端

### 8.1 网页版(PWA)

MVP 的页面:

- **会话列表:** 按 agent 和 cwd 分组,显示标题、状态、更新时间;等待授权的会话置顶。
- **会话页:** markdown 消息流、工具调用卡片(标题、类型、状态、diff)、plan、授权按钮、文本输入框、取消按钮。
- **自动重连:** 断开后重新 `session/load`,用重放的快照替换本地状态。

### 8.2 客户端核心逻辑规范

ACP 的消息流需要客户端处理后才能显示,每个客户端都要实现同样的规则:

| 更新 | 处理 |
|---|---|
| `user_message_chunk`、`agent_message_chunk`、`agent_thought_chunk` | 与上一条同类型、同 `messageId` 的文本块拼接;否则新起一条 |
| `tool_call` | 按 `toolCallId` 新建或覆盖 |
| `tool_call_update` | 合并到同 `toolCallId` 的工具调用;出现的字段覆盖原值,`content` 和 `locations` 整体替换 |
| `plan`、`current_mode_update`、`config_option_update`、`session_info_update`、`usage_update` | 只保留最新的值(`session_info_update` 按字段合并) |
| 未知类型 | 忽略,不报错 |
| 授权请求 | 显示选项;回答或收到 `$/cancel_request` 后移除 |
| `turn_state` | 更新"生成中"状态 |

这些规则和 daemon 的快照压缩规则(`crates/acp_mux/src/snapshot.rs`)一致,因此重放的快照可以按同样的方式处理。

为了让网页、iOS、Android 三端行为一致:

1. 把以上规则写成独立的规范文档;
2. 准备一组固定的消息样例(输入的 ACP 消息序列,加上期望的显示状态),作为三端共用的测试用例。

### 8.3 原生 App

两种做法,服务端都不需要改:

- **Capacitor 包装网页:** 复用全部代码,最快。
- **Swift / Kotlin 原生实现:** 按第 8.2 节的规范重写核心逻辑,用同一组样例验证。

如果以后三端的维护成本确实太高,再考虑在 gateway 加一层"视图模型"接口,把 ACP 转换成扁平的显示状态。现在不建议做:这是一个新的协议层,会把原方案 D 的维护问题带回来。

## 9. 已知问题和待确认项

| # | 项 | 说明 |
|---|---|---|
| 1 | 重放的数据量 | 每次重连都发送整个会话的快照,长会话在移动网络下可能较慢。以后可以加序号,只补发断开期间的增量 |
| 2 | 手机发起的消息在桌面上的显示 | 依赖第 6.2 节的广播和 Zed 对 `user_message_chunk` 的处理,需要实测桌面 Zed 中的显示效果 |
| 3 | 授权"先回答的生效" | 桌面 Zed 收到 `$/cancel_request` 后会移除授权提示,需要实测 |
| 4 | 会话标题 | 来自 agent 的 `session_info_update`。不发送标题的 agent,列表里只能显示 cwd 和时间 |
| 5 | 已退出的会话 | agent 进程退出后会话为 `dead`,只能查看,不能发消息 |
| 6 | 多个 Linux 主机 | 每台主机各有一个 daemon 和 gateway。客户端需要支持配置多个服务端地址 |

## 10. 阶段与工作量(粗估)

| 阶段 | 内容 | 工作量 |
|---|---|---|
| 0 | 真实环境验证 acp-mux(真实的 Zed 加真实的 agent) | 1–2 天 |
| 1 | 观察者握手;多端同时观察;单元测试和端到端测试 | 1 周 |
| 2 | gateway 和认证;网页 MVP(会话列表、消息流、发文本、批准、取消、自动重连) | 2 周 |
| 3 | 推送通知(ntfy、Web Push);diff 卡片;elicitation 表单;切换模式和模型 | 1–2 周 |
| 4(可选) | 会话持久化;增量重连 | 1–2 周 |
| 5 | 原生 App(Capacitor 包装,或 Swift / Kotlin 原生实现) | 视做法而定 |

阶段 1 和阶段 2 合计约 3 周,即第 1 节所说的 MVP。

## 11. 实现状态(2026-10-19)

使用方法见 [acp-mux 使用方法](../src/development/acp-mux.zh-CN.md) 的"网页和手机访问"和"通知"两节。

### 11.1 各阶段

| 阶段 | 状态 | 说明 |
|---|---|---|
| 0 | 未完成 | 还没有用真实的 Zed 和真实的 agent 验证 |
| 1 | 已完成 | 观察者握手(协议 2,daemon 仍接受协议 1 的 attach);多端同时观察;先回答的生效;一端发 prompt 时向其他端广播用户消息和轮次状态 |
| 2 | 已完成 | 网关、token 登录、网页 MVP(会话列表、消息流、发文字、批准或拒绝、停止、自动重连、PWA) |
| 3 | 大部分完成 | 已完成:ntfy 通知、diff 卡片、计划、elicitation 表单、切换模式和设置项。未做:Web Push、`POST /api/v1/push/register`、切换模型(`session/set_model`) |
| 4 | 未做 | 会话持久化、增量重连 |
| 5 | 未做 | 原生 App |

### 11.2 代码

| 位置 | 内容 |
|---|---|
| `crates/acp_mux/src/router.rs` | observer 改为集合;观察者 client(不绑定 agent);`session_key` 按 client 类型查找会话;授权请求转发给所有 observer,回答后向其他端发 `$/cancel_request`;产生通知事件 |
| `crates/acp_mux/src/control.rs` | `PROTOCOL_VERSION = 2`、`MIN_PROTOCOL_VERSION = 1`;`HandshakeRequest::Observe`;`list` 输出观察者数量 |
| `crates/acp_mux/src/daemon.rs` | 协议协商(按 client 的版本回复);观察者握手;启动网关和通知任务 |
| `crates/acp_mux/src/gateway.rs` | 新增。HTTP/1.1 和 WebSocket 网关、登录、Origin 检查、失败次数限制 |
| `crates/acp_mux/src/notify.rs` | 新增。通知事件、延迟发送、通过 curl 发布到 ntfy |
| `crates/acp_mux/web/` | 新增。网页客户端(原生 JavaScript,无构建步骤),通过 `include_str!` 编进二进制 |

### 11.3 与本文设计的差异

| # | 本文的设计 | 实际实现 | 原因 |
|---|---|---|---|
| 1 | 网关是单独的子命令 `acp-mux serve` | 网关运行在 daemon 进程内:配置 `gateway` 后由 daemon 启动,修改后执行 `acp-mux restart` | 不需要再管理一个进程;网关的生命周期和 daemon 一致;直接把 WebSocket 连接交给路由,不经过 unix socket 转一次 |
| 2 | 观察者握手 `{"acp_mux": 2, "observe": {}}` | 相同,另外可以带 `label`,用于日志 | 日志中区分来源 |
| 3 | — | daemon 用 client 的协议版本回复握手 | 旧版 attach 要求回复的版本和自己的相同 |
| 4 | `POST /api/v1/push/register`、Web Push、APNs / FCM | 未实现,只做了 ntfy | 原生 App 还没有;Web Push 需要 VAPID 签名和消息加密,工作量单独算 |
| 5 | ntfy 只配置 `ntfy_url` | 另有 `ntfy_token_file`,用于需要认证的主题 | 自建 ntfy 常开启认证;token 放在文件里,不写进配置 |
| 6 | 网关只做认证和帧转换 | 另有 `cookie_secure`(VPN 内用纯 http 时关闭)和 `allowed_origins`(代理改写 `Host` 时使用) | 浏览器不保存纯 http 下的 `Secure` cookie;frp 可能改写 `Host`,导致同源检查失败 |
| 7 | — | 通知通过 `curl` 发送 | 避免给 daemon 链接 TLS 库;请求内容从 stdin 传给 curl,token 不出现在进程列表里 |
| 8 | — | 登录凭据只保存在内存中,有效期 30 天 | daemon 重启后需要重新登录 |
| 9 | 轮次正在执行时 prompt 进入队列 | 相同(daemon 先取消当前轮次);网页在发送前会确认 | 避免误中断 |
| 10 | — | 网页只观察当前打开的会话,返回列表时发 `session/close` | 授权请求只发给正在看这个会话的地方 |

### 11.4 测试

| 范围 | 内容 | 结果 |
|---|---|---|
| `cargo test -p acp_mux` | 41 个测试。新增:观察者只能使用已有会话、观察者列出所有会话;多端观察和先回答的生效;一端发 prompt 时其他端收到用户消息和轮次状态;观察者断开或关闭后授权请求仍保留给其他端;通知事件;网关的页面、登录、失败次数限制、WebSocket 桥接、未登录和其他来源的拒绝;通知的延迟发送;unix socket 上的协议 1 attach 握手、观察者和 attach client 共享会话 | 全部通过 |
| clippy | acp_mux、remote_server,`--deny warnings` | 无警告 |
| Mac 浏览器端到端 | 本机运行 daemon 和网关,Python 脚本扮演 Zed,无头 Chrome 操作网页:错误 token 被拒绝、登录、列表显示"等待授权"、打开会话(重放、diff 卡片、计划、模式选择)、在网页上批准、agent 继续直到空闲、从网页发消息;Zed 一端收到 `$/cancel_request`、镜像的用户消息和轮次状态;浏览器控制台无错误 | 通过 |
| Mac 通知 | 本地 HTTP 服务代替 ntfy:延迟 2 秒后发出"等待授权"(中文标题经 URL 编码,带 Bearer token),轮次结束后发出"已完成" | 通过 |
| t228(Linux) | 重新编译 remote_server;acp_mux_implementation.md 11.7 节的驱动程序(第 5 步改为验证多端观察)27/27 通过;经 SSH 隧道用无头 Chrome 重复上面的网页流程;ntfy 通知 | 全部通过 |

### 11.5 尚未验证

- 真实的 Zed 客户端:另一端发 prompt 时,Zed 线程中能否正确显示镜像过来的用户消息和"生成中"(第 9 节第 2 项);Zed 收到 `$/cancel_request` 后授权提示是否消失(第 9 节第 3 项)。
- 真实的 agent(Claude Code、Codex 等)。
- frp 和真实的 ntfy 服务;iOS Safari 上添加到主屏幕。

