---
title: "远程开发中的 Codex active writer 冲突"
description: "诊断并恢复 Codex thread already has an active writer 错误，以及 Zed 远程 ACP 生命周期中的相关缺口。"
---

# 远程开发中的 Codex active writer 冲突

本文说明 Zed 远程开发中以下错误的来源、诊断方法、恢复步骤和长期修复方向：

```text
Internal error: {
  "details": "thread <uuid> already has an active writer"
}
```

本文基于 Zed commit `36dd5531680f1bbd57002dbcb42d76ac4d87a68f`
（`crates/zed` 版本 `1.17.0`）、`@agentclientprotocol/codex-acp` `1.6.2`
及其依赖的 `@openai/codex` `0.148.0` 复核。Agent Registry 和 Codex 会独立
更新，实际行为应以发生问题时安装的版本为准。

配套阅读：[Zed 远程开发通信与 ACP 代理机制](./remote-development-internals.zh-CN.md)。

## 结论 {#conclusion}

这条错误来自 **OpenAI Codex thread store**，不是 Zed 生成的错误，也不是 ACP
协议定义的通用锁错误。

Codex 为每个可写 thread 持有一个操作系统文件锁。默认锁文件是：

```text
~/.codex/thread-writer-locks/<thread-id>.lock
```

如果设置了 `CODEX_HOME`，锁目录位于：

```text
$CODEX_HOME/thread-writer-locks/<thread-id>.lock
```

另一个 Codex 进程尝试恢复同一个 thread 时，`File::try_lock()` 返回
`WouldBlock`，Codex 随即返回：

```text
thread <uuid> already has an active writer
```

错误只证明“另一个进程仍持有该 thread 的 writer lock”，不能单凭错误断定持锁
进程一定是 Zed 断连留下的孤儿。持有者也可能是 Codex CLI、Codex Desktop、
Remote Control、编辑器扩展、另一个 Zed 窗口或另一条远程会话。

## Zed 中的触发链路 {#zed-trigger-chain}

远程项目中的 Codex Agent 实际运行在远程主机：

1. 远端 `AgentServerStore` 解析、安装并返回 Agent 命令。
2. host Zed 把远端命令包装为一个新的 SSH exec channel。
3. 远端 `codex-acp` 启动 `codex app-server`。
4. Zed 的 ACP `session/load` 或 `session/resume` 最终由 `codex-acp` 转成 Codex
   `thread/resume`。
5. `thread/resume` 获取对应 thread 的 writer lock。

正常情况下，同一个 `codex app-server` 可以让新的客户端重新加入已经加载的 thread。
冲突发生在另一个 Codex 进程仍持有锁，而当前进程尝试冷恢复同一 thread 时。

远程网络分区可能形成下面的路径：

```text
网络分区
 ├─ host：主 RPC 链路检测失败并重连
 └─ remote sshd：尚未判定旧 SSH session 死亡
      └─ 旧 codex-acp / codex app-server 继续运行并持有 writer lock

Zed 显式重建 Agent connection 或 reload workspace
 └─ 新 SSH channel 启动新的 codex-acp / codex app-server
      └─ Zed 使用原 session id 调用 session/load
           └─ codex-acp 调用 thread/resume
                └─ 新进程无法取得旧进程持有的 writer lock
```

`ConversationView::reset()` 本身不保证创建新 Agent 进程。它会调用
`AgentConnectionStore::request_connection`，而后者在存在缓存 entry 时直接返回。只有
显式 `restart_connection`、连接建立失败、新版本到来、Agent 从远程列表消失或整个
workspace/project 被重建时，缓存连接才可能被替换。

## 安全诊断 {#diagnosis}

### 1. 确认锁所在主机

对于 Zed SSH 远程项目，Registry Agent 的命令和 Codex 数据目录通常都在远程
主机，因此以下命令应在**远程主机**执行。若 Agent 是本地项目启动的，则在本机
执行。

### 2. 查找准确的持锁进程

以 thread `01a01a4a-d5fc-77a0-93a4-52cd3793ebd9` 为例，默认
`CODEX_HOME` 下可执行：

```sh
lsof "$HOME/.codex/thread-writer-locks/01a01a4a-d5fc-77a0-93a4-52cd3793ebd9.lock"
```

Linux 上没有 `lsof` 时可使用：

```sh
fuser -v "$HOME/.codex/thread-writer-locks/01a01a4a-d5fc-77a0-93a4-52cd3793ebd9.lock"
```

得到 PID 后检查进程及其父进程：

```sh
ps -o pid,ppid,pgid,etime,args -p <PID>
```

下面的命令可以辅助查看相关进程，但不能据此直接批量终止：

```sh
pgrep -af 'codex-acp|codex.*app-server'
```

一台远程主机可能同时承载多个正常的 Zed workspace 或 Codex 客户端。应以锁文件
持有者及其进程树为准，避免使用无差别的 `pkill codex`。

### 3. 区分 active lock 和 stale lock 文件

锁文件存在不等于锁仍被持有：

- `lsof` 或 `fuser` 能找到持有者：存在 active writer。
- 找不到持有者：文件可能只是无主的 stale lock 文件。

当前 Codex 会在首次获取 writer lock 时尝试清理没有被任何进程锁定的 stale lock
文件，因此后一种情况通常只需重试，不需要手动删除文件。

## 恢复步骤 {#recovery}

### 情况 A：持锁进程是仍在使用的正常客户端

关闭或停止使用对应的 Codex CLI、Desktop、Remote Control、编辑器扩展或另一
Zed 会话，让原客户端正常释放 thread。确认锁不再有持有者后，再在 Zed 中重试。

### 情况 B：持锁进程是断连留下的孤儿

先尝试正常终止准确的持锁进程：

```sh
kill -TERM <PID>
```

再次运行 `lsof` 或 `fuser` 确认锁是否释放。只有确认进程无法退出时才使用：

```sh
kill -KILL <PID>
```

如果持锁的 `codex app-server` 由已经成为孤儿的 `codex-acp` 启动，应检查父子
进程关系并清理对应的旧进程树，而不是终止主机上的所有 Codex 进程。

锁释放后，在 Zed 中执行 {#action workspace::Reload}。

也可以关闭并重新打开远程项目，然后重新打开原 thread。只点击
`Restart Connection` 不一定能恢复已经处于 `LoadError` 的 conversation，因为
连接缓存和 conversation 状态可能已经分离。

### 情况 C：无法停止原持有者

新建一个 thread。新 thread 使用新的 UUID，不会争用原 writer lock，但不会自动
继承原 thread 的完整上下文。

### 不要删除仍被持有的锁文件

不要把下面的操作作为恢复手段：

```sh
rm ~/.codex/thread-writer-locks/<thread-id>.lock
```

在 Unix 上，删除仍被进程打开并锁定的文件不会释放旧 inode 上的锁。新进程可能
创建同名的新文件并锁定另一个 inode，从而绕过单写者保护，让两个进程并发写同一
thread，带来 JSONL 或 SQLite 状态损坏风险。

## SSH 保活只能缓解远程孤儿 {#ssh-keepalive}

如果有远端 sshd 管理权限，可以配置：

```text
ClientAliveInterval 15
ClientAliveCountMax 3
```

修改后应先验证配置：

```sh
sshd -t
```

再按操作系统的服务管理方式 reload sshd。上述值只是示例，应结合组织的网络和
安全策略调整。

host 的 SSH 配置也可以加入：

```text
Host <Zed 使用的远程主机别名>
    ServerAliveInterval 15
    ServerAliveCountMax 3
```

两者作用不同：

- `ClientAliveInterval` 让远端 sshd 检测失联客户端，有助于回收远端 session。
- `ServerAliveInterval` 让本地 SSH 客户端检测失联服务端，有助于避免本地进程
  长时间挂起。

这只是远程网络分区场景的缓解措施，不是 `active writer` 的通用根治方案。它不能
解决 Desktop、CLI、Remote Control 或多个编辑器实例同时恢复同一 Codex thread
的冲突，也不能替代 Zed 对 ACP 远端进程的生命周期治理。

## Zed 当前的生命周期缺口 {#zed-lifecycle-gaps}

### 死亡连接仍可能保留在连接缓存中

`AcpConnection` 的 child status task 会把 `LoadError::Exited` 广播给已注册的
session，但不会同步把 `AgentConnectionStore.entries` 中已经成功建立的 entry
改为 `Error` 或移除。后续 `request_connection` 可能继续返回一个逻辑上为
`Connected`、实际上已经不可用的连接。

相关实现：

- [`AcpConnection`](../../../crates/agent_servers/src/acp.rs)
- [`AgentConnectionStore`](../../../crates/agent_ui/src/agent_connection_store.rs)
- [`ConversationView::reset`](../../../crates/agent_ui/src/conversation_view.rs)

### ACP 没有独立的心跳和自动重连

主 RPC 链路有 Ping、超时、重连和 Envelope 重放；ACP 使用独立的
newline-delimited JSON-RPC stdio，没有等价的心跳、重连和消息重放。用户可见的
断连检测主要依赖 SSH/Agent 子进程退出。

### 正常 close 无法跨越已断开的通道

Zed 在离开 connected state 时会尝试调用 ACP `session/close`。`codex-acp` 会将
其转换为 Codex `thread/unsubscribe`，但网络分区时旧 Agent 收不到该请求。即使
当前连接已重建，也无法通过新进程向旧进程发送 unsubscribe。

## 建议的工程修复 {#engineering-fixes}

### 1. 让 Agent 进程退出更新连接缓存

当 `AcpConnection` 的 child status 完成时，应通知 `AgentConnectionStore`，将对应
entry 原子地转为 `Error` 或移除。后续请求才能建立真正的新连接，而不是复用死亡
entry。

### 2. 将主远程连接状态与 ACP entry 关联

主 `RemoteClient` 进入 `Reconnecting` 或 `Disconnected` 时，应标记依赖该远程
连接的 ACP entry 为失效。主链路恢复后按需重建 Agent connection。

### 3. 为远端 Agent 增加 supervisor 或 takeover

可靠的远端进程治理至少需要：

- 为 Agent 实例记录 PID、进程启动时间和 generation；
- 新实例启动前验证并终止旧实例的完整进程树；
- 等待旧进程退出及 writer lock 释放后再恢复 session；
- 防止 PID 复用导致误杀；
- 将 `codex-acp` 及其 `codex app-server` 子进程置于同一生命周期边界。

这比无差别 `pkill` 或仅依赖 SSH SIGHUP 更可靠。

### 4. 在 UI 中识别 active-writer 冲突

对 JSON-RPC code `-32600` 且消息匹配
`thread <id> already has an active writer` 的错误，应提供专用恢复界面：

- 说明 thread 正被另一个 Codex 进程使用；
- 提供“重试原 thread”；
- 提供“新建 thread”；
- 在支持安全 fork 时提供“从历史创建新 thread”；
- 避免无提示地循环恢复同一个 UUID。

### 5. 为 `LoadError` 提供真正的重试入口

当前 `render_load_error` 主要显示错误和复制按钮。重试操作应先使连接缓存失效并
建立新 Agent connection，再恢复原 session；不能只重新读取已有 entry。

## 已验证的上游依据 {#references}

- [Zed #60213：ACP agents can't be recovered after SSH disconnect](https://github.com/zed-industries/zed/issues/60213)
- [OpenAI Codex #37403：Desktop/CLI active writer 冲突](https://github.com/openai/codex/issues/37403)
- [OpenAI Codex #38144：fork 后父 thread 保留 writer](https://github.com/openai/codex/issues/38144)
- [Codex writer lock 实现](https://github.com/openai/codex/blob/main/codex-rs/thread-store/src/local/writer_lock.rs)
- [Codex 跨进程 writer 冲突测试](https://github.com/openai/codex/blob/main/codex-rs/app-server/tests/suite/v2/thread_resume.rs)
- [codex-acp 的 resume/load 实现](https://github.com/agentclientprotocol/codex-acp/blob/main/src/CodexAcpClient.ts)
- [codex-acp 的 session close 实现](https://github.com/agentclientprotocol/codex-acp/blob/main/src/CodexAcpServer.ts)
