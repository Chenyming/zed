# acp-mux 使用方法

acp-mux 让 SSH 远程项目里的外部 ACP agent 在 Linux 服务端常驻运行，像 tmux 一样
可以随时断开（detach）和重新连接（attach）：

- 在 agent 线程里发起任务后，可以关掉 Zed、合上笔记本或断网，agent 在服务端继续执行。
- 重新连接并打开这个线程，可以看到断开期间产生的消息、工具调用和 diff，以及等待授权的请求。
- 如果 agent 还在执行，线程显示"生成中"，可以停止，结束时有通知（需要 Mac 端 Zed 也包含 acp-mux 的修改，见[版本要求](#requirements)）。

设计和实现细节见 `docs/zed-mirror/acp_mux_implementation.md`。

## 哪些 agent 会接入 {#agents}

### 外部 agent：开启后接入

acp-mux 不维护白名单。开启后，SSH 远程项目里由 Zed 启动的外部 ACP agent 都会接入，
包括 ACP 注册表中的 agent 和自定义 agent。agent 的名称就是 Zed 设置
`agent_servers` 中的键名，内置的几个名称是：

| 名称         | agent                           |
| ------------ | ------------------------------- |
| `claude-acp` | Claude Agent（Claude Code）     |
| `codex-acp`  | Codex                           |
| `gemini`     | Gemini CLI                      |
| `cursor`     | Cursor                          |
| 其他         | 注册表 agent 的 id 或自定义名称 |

不确定名称时，可以先不限制 agent，连接后执行 `acp-mux list`（见[管理命令](#commands)），
第一列就是名称。

验证情况：目前只用脚本化的假 agent 在 Linux 上验证过完整流程，**还没有用以上真实 agent
测试过**。agent 需要能在不使用 Zed 的文件读写和终端能力的情况下工作（用自己的工具读写
文件、执行命令），常见的 agent 都是这样，但需要逐个实测。

### Zed 内置 agent：不接入

Zed 内置的 agent（Zed Agent）运行在 Mac 上的 Zed 进程里，通过远程项目读写 Linux 上的
文件，不会在 Linux 上启动 agent 进程，所以**始终不经过 acp-mux**，断开连接后任务会停止。
这一点与配置无关。

### 其他不接入的情况

- 本地项目：agent 直接在 Mac 上运行，不经过 remote server。
- Windows 远端：acp-mux 只在 Unix 上编译。
- WSL 和 Docker 连接也使用 remote server，在其中放置配置文件同样会开启，但没有验证过。
- 配置文件不存在，或没有设置 `"enabled": true`。
- 设置了 `agents`，但 agent 名称不在列表里。

## 版本要求 {#requirements}

| 端                  | 要求                                                            | 缺少时                                                                    |
| ------------------- | --------------------------------------------------------------- | ------------------------------------------------------------------------- |
| Linux remote_server | 包含提交 `090fc48595` 或更新（修复了 Linux 上回复被卡住的问题） | 旧版本的 acp-mux 会让 agent 无法完成初始化，必须重新编译                  |
| Mac 端 Zed          | 包含提交 `32a87900f1` 或更新                                    | 仍可断开和重连、看到完整内容，但重连时如果 agent 仍在执行，线程显示为空闲 |

编译和上传 remote_server 的方法见 [独立编译 Linux Remote Server](./linux-remote-server-build.zh-CN.md)。

## 开启 {#enable}

在 **Linux 服务端** 创建 `~/.config/zed/acp_mux.json`（设置了 `XDG_CONFIG_HOME`
时为 `$XDG_CONFIG_HOME/zed/acp_mux.json`）。注意不是 Mac 上的文件，也不是 Zed 的
`settings.json`。

最简配置，所有外部 agent 都接入：

```json
{
  "enabled": true
}
```

只接入指定的 agent：

```json
{
  "enabled": true,
  "agents": ["claude-acp", "codex-acp"]
}
```

自动批准某些类型的工具调用（daemon 直接选择"允许一次"，不再询问）：

```json
{
  "enabled": true,
  "agents": ["claude-acp"],
  "auto_approve": ["read", "search"]
}
```

| 字段           | 类型       | 默认值  | 说明                                                                                                                                                                     |
| -------------- | ---------- | ------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `enabled`      | 布尔       | `false` | 是否开启。只有 `true` 才生效                                                                                                                                             |
| `agents`       | 字符串数组 | 不限制  | 接入的 agent 名称。省略时所有外部 agent 都接入                                                                                                                           |
| `auto_approve` | 字符串数组 | `[]`    | 自动批准的工具类型，取值为 ACP 的 tool kind：`read`、`edit`、`delete`、`move`、`search`、`execute`、`think`、`fetch`、`switch_mode`、`other`。每次自动批准都会记录到日志 |

生效时机：

- `enabled` 和 `agents`：每次建立远程连接时读取。修改后，在 Zed 中断开并重新连接 Linux 才生效。
- `auto_approve`：daemon 启动时读取。修改后执行 `acp-mux restart` 才生效。

配置文件格式错误时，远程连接会忽略它并按未开启处理，错误记录在 remote server 的日志里。

建议先只接入一个 agent，用短任务试一下：发起任务 → 退出 Zed → 重新打开线程 →
确认内容完整、能继续对话。

## 日常使用 {#usage}

开启后不需要额外操作，在 Zed 中照常使用 agent：

- **断开**：直接关掉 Zed、断网或关闭远程项目都可以。agent 继续执行；需要授权的请求会挂起，等待下次连接。
- **重新连接**：连接 Linux 后，从 agent 的线程历史中打开之前的线程。会先重放全部内容，再补发等待授权的请求；agent 仍在执行时显示"生成中"。
- **多个窗口**：同一个线程同时只能在一个窗口里实时更新。后打开的窗口会接管这个线程，先打开的窗口停止更新，挂起的授权请求转到新窗口。
- **停止任务**：在线程中点击停止，和不使用 acp-mux 时一样。

## 管理命令 {#commands}

在 Linux 上执行。`<remote_server>` 是服务端二进制的路径，可以从
`~/.zed_server/` 中找到，例如 `~/.zed_server/zed-remote-server-stable-<版本>`。

```sh
<remote_server> acp-mux list                    # 列出 agent 和会话
<remote_server> acp-mux kill <会话 id>          # 结束一个会话
<remote_server> acp-mux kill --agent <名称>     # 结束一个 agent 的所有进程
<remote_server> acp-mux restart                 # 重启 daemon，会中断所有正在执行的任务
```

`list` 的输出示例：

```text
AGENTS
  claude-acp           pid 1162998  clients 0   sessions 1   /usr/bin/node
SESSIONS
  0b9c…                                  claude-acp       waiting  detached  2026-10-18T07:11:13Z /home/me/project 修复登录问题
```

会话状态：

| 状态      | 含义                                               |
| --------- | -------------------------------------------------- |
| `idle`    | 空闲                                               |
| `running` | 正在执行                                           |
| `waiting` | 有等待回答的授权请求                               |
| `dead`    | agent 进程已退出。仍可打开查看内容，但不能继续对话 |

`attached` / `detached` 表示当前是否有 Zed 窗口连着这个会话。

## 运行方式 {#how-it-works}

```text
Mac 上的 Zed ──ssh──> remote_server acp-mux attach ──unix socket──> acp-mux daemon ──stdio──> agent 进程
                      （随 SSH 连接创建和退出）                    （每个用户一个，常驻）    （常驻）
```

开启后，remote server 把 agent 的启动命令改写为 `remote_server acp-mux attach --agent <名称> -- <原命令>`。
attach 连接 daemon，daemon 不存在时自动启动。daemon 为每个 agent 保留一个长期运行的进程，
并记录每个会话的内容，供重新连接时重放。

| 项            | 位置                                                                      |
| ------------- | ------------------------------------------------------------------------- |
| 配置文件      | `~/.config/zed/acp_mux.json`                                              |
| daemon 日志   | `~/.local/share/zed/logs/acp-mux.log`                                     |
| socket 和 pid | `~/.local/share/zed/server_state/acp-mux/`（权限 0700，只有本用户可连接） |

设置了 `XDG_CONFIG_HOME` 或 `XDG_DATA_HOME` 时，以上路径相应改变。

## 与不使用 acp-mux 的区别 {#limitations}

- **改动审阅不可用**：agent 直接在 Linux 上写文件，不经过 Zed，所以"审阅、保留或撤销改动"不可用。工具调用里附带的 diff 仍然可以看到。
- **没有内嵌终端**：agent 自己执行命令，输出以工具调用内容的形式显示。
- **agent 进程常驻**：断开所有连接后 agent 也不会退出，会一直占用内存，需要时用 `acp-mux kill --agent` 结束。
- **会话不持久化**：daemon 重启（`acp-mux restart`、服务器重启）后，会话内容只保存在内存里的部分会丢失。agent 自己支持加载历史会话时，仍可以从 agent 恢复。
- **登录**：agent 需要登录时，第一次连接时在 Zed 中完成，凭据由 agent 保存在 Linux 上。在终端中执行的登录命令不经过 acp-mux。

## 升级 {#upgrade}

上传新版本的 remote_server 后，正在运行的 daemon 和 agent 不受影响，继续使用旧版本。
新旧版本的协议兼容时，新连接会直接使用旧 daemon；不兼容时，agent 会启动失败并提示执行
`acp-mux restart`。要让新版本生效，在没有正在执行的任务时执行一次：

```sh
<新的 remote_server> acp-mux restart
```

## 关闭 {#disable}

1. 删除 `~/.config/zed/acp_mux.json`，或把 `enabled` 改为 `false`。
2. 在 Zed 中断开并重新连接 Linux。之后启动的 agent 不再接入 acp-mux。
3. 结束已经在运行的 agent 和 daemon：

   ```sh
   <remote_server> acp-mux kill --agent <名称>
   kill "$(cat ~/.local/share/zed/server_state/acp-mux/daemon.pid)"
   ```

## 常见问题 {#troubleshooting}

| 现象                                          | 处理                                                                                                          |
| --------------------------------------------- | ------------------------------------------------------------------------------------------------------------- |
| 开启后 agent 一直启动不了，没有任何回复       | remote_server 版本太旧，带有 Linux 上回复被卡住的 bug，见[版本要求](#requirements)                            |
| 断开后任务还是停了                            | 确认配置文件在 Linux 上、`enabled` 为 `true`，并且修改后重新连接过；执行 `acp-mux list` 看 agent 是否在列表中 |
| agent 启动失败，提示协议版本不兼容            | 执行 `acp-mux restart`                                                                                        |
| 重新连接后看不到之前的线程                    | 从 agent 的线程历史中打开；如果 daemon 重启过，内容可能已丢失                                                 |
| 重新连接后线程显示空闲，但 agent 其实还在执行 | Mac 端 Zed 不包含 acp-mux 的修改，见[版本要求](#requirements)                                                 |
| 会话状态是 `dead`                             | agent 进程已退出，查看 `acp-mux.log`；新建线程会启动新的 agent 进程                                           |
| 用户退出登录后 daemon 被杀掉                  | 部分发行版设置了 systemd 的 `KillUserProcesses=yes`，可执行 `loginctl enable-linger`                          |

更多信息见 daemon 日志 `~/.local/share/zed/logs/acp-mux.log`。
