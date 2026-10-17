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
| `--no-strip`        | 只编译，不生成去掉调试信息的产物和压缩包     | 生成                                       |
| `--help`            | 显示帮助                                     | 不执行构建                                 |

例如构建 musl 目标，或使用与客户端一致的 preview 通道：

```sh
./script/build-linux-remote-server.sh --target x86_64-unknown-linux-musl
./script/build-linux-remote-server.sh --channel preview
```

支持 `x86_64` 和 `aarch64` 的 GNU / musl 目标。非原生目标需要另行安装
Rust target、C/C++ 工具链和链接器，脚本不会自动安装这些依赖。

## 产物与验证 {#artifacts}

release 配置保留了调试信息（`debug = "limited"`），直接编译出的二进制有数百 MB。
脚本编译后与官方发布包一样，用 objcopy 去掉调试信息和局部符号，再用 gzip 压缩：

| 文件                                                         | 用途                                   |
| ------------------------------------------------------------ | -------------------------------------- |
| `target/remote-server-dist/<target-triple>/remote_server`    | 去掉调试信息，用于上传和部署           |
| `target/remote-server-dist/<target-triple>/remote_server.gz` | 上面文件的 gzip 压缩包，体积最小       |
| `target/<target-triple>/release/remote_server`               | 未去掉调试信息，仅用于本地调试和符号化 |

objcopy 依次查找 `llvm-objcopy`、`llvm-objcopy-<版本>`、Rust 工具链自带的
`rust-objcopy` / `llvm-objcopy`，最后才使用 GNU `objcopy`。较旧发行版的 GNU objcopy
不认识新版 LLVM 生成的 CREL 节，失败时请安装 llvm，或执行
`rustup component add llvm-tools`。使用 `--no-strip` 时只生成最后一个文件。

也可通过 `CARGO_TARGET_DIR` 指定输出文件夹。临时文件位于仓库的 `docs/tmp/`。

在目标 Linux 上执行产物的 `version` 和 `--help`，确认版本及 `exec` 子命令。
以默认输出文件夹中的 x86_64 GNU 产物为例：

```sh
./target/remote-server-dist/x86_64-unknown-linux-gnu/remote_server version
./target/remote-server-dist/x86_64-unknown-linux-gnu/remote_server --help
```

编译完成后仍需自行部署。远端文件名以客户端实际请求的通道和完整版本为准，
不要将新产物伪装成旧的官方版本。

## 从 macOS 客户端自动上传 {#upload-from-macos}

可以把 Linux 产物复制到 Mac，由客户端自动上传、命名并设置远端文件的执行权限，
无需手动命名或安装到远端。在本地用户 `settings.json` 中配置：

```json [settings]
{
  "copy_remote_server": "/Volumes/code/zed/tmp/remote_server"
}
```

这是 Mac 上的文件路径，支持 `~/`，不是 Linux 上的安装路径。
原始二进制和 `.gz` 都受支持，文件必须匹配远端 Linux 架构和客户端源码。
该设置在默认 release 构建中也可用，无需启用额外 feature。
保存后在下一次新建远程连接时生效；已有连接需要先关闭后重新连接。

`copy_remote_server` 优先于 `ZED_COPY_REMOTE_SERVER` 和
`ZED_REMOTE_SERVER_VERSION`，使用客户端自身的通道和版本命名远端文件。
路径不存在、指向文件夹、文件无法读取、上传失败或校验失败时，连接会停止，
并在连接错误界面显示原因，不会回退到源码编译、已有服务端或官方下载。
删除该设置或设为 `null` 后，仍可使用原来的 `ZED_COPY_REMOTE_SERVER` 环境变量。

如果还需要从源码自动构建服务端，可在 Mac 上启用原有构建选项：

```sh
./script/build-mac.sh --channel stable --with-copy-remote-server
```

`--with-copy-remote-server` 启用 `remote/build-remote-server-binary` feature，
自动禁用官方服务端版本查询和版本覆盖；与非空 `ZED_REMOTE_SERVER_VERSION`
同时使用会报错。该参数只启用客户端能力，不编译 macOS 或 Linux 服务端。
它与额外编译 macOS 服务端的 `--with-remote-server` 是两个独立选项。

仅编译二进制时也可使用该参数，但不会更新已安装的 `.app`：

```sh
./script/build-mac.sh binaries --channel stable --with-copy-remote-server
```

直接使用底层打包脚本时，对应参数为 `./script/bundle-mac -c`。

将 Linux 产物（推荐 `remote_server.gz`）放到 Mac 上的固定路径，例如
`/Volumes/code/zed/tmp/remote_server`。原始二进制和 `.gz` 都受支持，文件必须
匹配远端 Linux 架构和客户端源码。彻底退出所有 Zed 进程后，在 Mac 上启动：

```sh
ZED_REMOTE_SERVER_VERSION= \
ZED_COPY_REMOTE_SERVER=/Volumes/code/zed/tmp/remote_server \
ZED_BUILD_REMOTE_SERVER=never \
/Applications/Zed.app/Contents/MacOS/zed
```

随后正常连接 Linux。环境变量同样只需在启动时指定，不需要在构建时提供；
配置了 `copy_remote_server` 时使用配置文件中的路径。

### 自动复用与更新 {#copy-cache}

SSH 连接到 Linux 或 macOS 时，客户端按解压后的二进制内容计算 SHA-256。
原始文件直接计算；`.gz` 流式解压后计算，不生成本地解压文件。
仅改变 gzip 时间戳或压缩方式、不改变二进制内容时，不会重复上传。

摘要缓存在当前 Zed 进程内，不写入本地缓存文件。未变化的同一个文件只计算一次，
并发连接也复用计算结果；文件大小、修改时间或文件标识变化时重新计算。
退出 Zed 后，进程内缓存消失，下次启动首次使用时重新计算。

远端安装成功后，在二进制旁保存 `.copy-sha256` 记录，包含内容摘要和安装文件标识。
后续连接中，远端程序可运行、摘要及文件标识一致时直接复用，可以始终保留
`copy_remote_server`。文件或记录缺失、内容变化或安装文件标识不符时重新上传。
首次使用这项缓存功能时，原先已上传但没有记录的文件需要重新上传一次。

上传后先校验解压后的 SHA-256，并确认二进制的 `version` 命令可运行，再替换
远端文件和记录。摘要不符或程序无法运行时保留原来安装的文件。
远端需要 `stat`、`sha256sum` 或 `shasum`；上传 `.gz` 时还需要 `gunzip`。
原子替换二进制不会主动终止已经运行的服务端进程，需要时关闭旧会话后重新连接。

需要强制重新上传时，在 Mac 启动命令中设置：

```sh
ZED_REMOTE_SERVER_VERSION= \
ZED_COPY_REMOTE_SERVER=/Volumes/code/zed/tmp/remote_server \
ZED_COPY_REMOTE_SERVER_FORCE=1 \
ZED_BUILD_REMOTE_SERVER=never \
/Applications/Zed.app/Contents/MacOS/zed
```

`ZED_COPY_REMOTE_SERVER_FORCE` 接受 `1` / `true` 或 `0` / `false`，默认不强制。
它只强制上传，不要求未变化的本地文件重新计算摘要。
Windows SSH、WSL 和容器连接暂时保留原有上传行为。

> **Warn:** 该 feature 也包含从源码自动构建服务端的路径。未配置上传文件时，
> 可通过 `ZED_BUILD_REMOTE_SERVER=never` 禁用自动编译。
> 已配置的上传文件无效时直接报错，不会触发自动编译。

## 远程 ACP 版本兼容 {#acp-compatibility}

当前客户端启动远程 ACP 需要服务端支持 `exec`。官方 stable `1.23.2`
不支持该子命令，会出现 `unrecognized subcommand 'exec'`。

部署同源服务端后，仍需在客户端禁用旧的 `ZED_REMOTE_SERVER_VERSION` 覆盖。
彻底退出所有 Zed 进程后，启动 macOS 客户端：

```sh
ZED_REMOTE_SERVER_VERSION= /Applications/Zed.app/Contents/MacOS/zed
```

仅禁用覆盖不会自动生成或部署匹配的 Linux 服务端。
