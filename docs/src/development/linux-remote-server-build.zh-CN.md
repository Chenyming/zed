# 独立编译 Linux Remote Server

在 Linux 上使用 `script/build-linux-remote-server.sh`，仅编译 Zed 的
`remote_server` 及其依赖，不编译客户端或 CLI，不打包，也不自动部署。
使用与 macOS 客户端相同的源码和通道，避免远程 ACP 启动时发生版本不兼容。

## 准备环境 {#prerequisites}

脚本仅在 Linux 上运行，不用于 macOS 交叉编译。准备仓库要求的 Rust 工具链
和 Linux 构建依赖，参见 [Linux 开发环境](./linux.md)。

Linux 上的源码必须与当前客户端一致，包括相关未提交修改。仅 checkout
同名分支不能保证一致，尤其是在本地修改尚未提交或推送时。

## 编译命令 {#build}

在仓库根目录执行：

```sh
./script/build-linux-remote-server.sh
```

默认使用 `stable` 通道和本机 Rust host 目标，以 release 模式编译。
脚本不改动 `crates/zed/RELEASE_CHANNEL`，不查询官方发布版本。

内存有限时，可限制编译并发：

```sh
./script/build-linux-remote-server.sh --jobs 2
```

## 可选参数 {#options}

| 参数                | 用途                                         | 默认值                                     |
| ------------------- | -------------------------------------------- | ------------------------------------------ |
| `--channel CHANNEL` | 指定 `dev`、`nightly`、`preview` 或 `stable` | `stable`                                   |
| `--target TRIPLE`   | 指定 Linux Rust 目标三元组                   | `TARGET_TRIPLE`，未设置时使用 `rustc` host |
| `--jobs NUMBER`     | 指定正整数编译并发数                         | 沿用 Cargo 配置                            |
| `--help`            | 显示帮助                                     | 不执行构建                                 |

例如构建 musl 目标，或使用与客户端一致的 preview 通道：

```sh
./script/build-linux-remote-server.sh --target x86_64-unknown-linux-musl
./script/build-linux-remote-server.sh --channel preview
```

支持 `x86_64` 和 `aarch64` 的 GNU / musl 目标。非原生目标需要另行安装
Rust target、C/C++ 工具链和链接器，脚本不会自动安装这些依赖。

## 产物与验证 {#artifacts}

默认产物路径：

```text
target/<target-triple>/release/remote_server
```

也可通过 `CARGO_TARGET_DIR` 指定输出文件夹。临时文件位于仓库的 `docs/tmp/`。

在目标 Linux 上执行产物的 `version` 和 `--help`，确认版本及 `exec` 子命令。
以默认输出文件夹中的 x86_64 GNU 产物为例：

```sh
./target/x86_64-unknown-linux-gnu/release/remote_server version
./target/x86_64-unknown-linux-gnu/release/remote_server --help
```

编译完成后仍需自行部署。远端文件名以客户端实际请求的通道和完整版本为准，
不要将新产物伪装成旧的官方版本。

## 远程 ACP 版本兼容 {#acp-compatibility}

当前客户端启动远程 ACP 需要服务端支持 `exec`。官方 stable `1.23.2`
不支持该子命令，会出现 `unrecognized subcommand 'exec'`。

部署同源服务端后，仍需在客户端禁用旧的 `ZED_REMOTE_SERVER_VERSION` 覆盖。
彻底退出所有 Zed 进程后，启动 macOS 客户端：

```sh
ZED_REMOTE_SERVER_VERSION= /Applications/Zed.app/Contents/MacOS/zed
```

仅禁用覆盖不会自动生成或部署匹配的 Linux 服务端。
