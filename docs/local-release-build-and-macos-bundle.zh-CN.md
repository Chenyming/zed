# 本地编译并打包 Zed macOS Release

本文记录在 Apple Silicon Mac 上从 Zed 仓库生成 release 二进制、`Zed.app` 和 DMG 的命令。

## 推荐的一键命令

在仓库根目录执行：

```sh
./script/build-mac.sh
```

该入口自动设置终端和临时目录，并查询最新官方 stable remote-server 版本。
默认仅编译客户端和 CLI；Linux remote-server 在连接远端时下载。

该脚本依次完成：

1. 生成第三方依赖许可证清单。
2. 以 release 模式编译 `zed` 和 `cli`。
3. 生成并签名 `Zed.app`。
4. 下载并加入应用内置的 Git 二进制。
5. 生成 `Zed-aarch64.dmg`。
6. 向 DMG 添加许可协议。

脚本内部对应的主要编译命令为：

```sh
cargo build --release \
  --package zed \
  --package cli \
  --target aarch64-apple-darwin
```

需要同时编译 macOS remote-server 时，显式启用：

```sh
./script/build-mac.sh --with-remote-server

# 只编译二进制，不打包 .app 和 DMG
./script/build-mac.sh binaries --with-remote-server
```

直接使用打包脚本时，对应参数为 `./script/bundle-mac -r`。
该选项额外执行以下 macOS 编译命令，不进行 Linux 交叉编译：

```sh
cargo build --release \
  --package remote_server \
  --target aarch64-apple-darwin
```

## 产物路径

完整流程结束后，主要产物位于：

```text
target/aarch64-apple-darwin/release/dmg/Zed.app
target/aarch64-apple-darwin/release/Zed-aarch64.dmg
```

release 二进制位于：

```text
target/aarch64-apple-darwin/release/zed
target/aarch64-apple-darwin/release/cli
```

启用 `--with-remote-server` 时，额外生成
`target/aarch64-apple-darwin/release/remote_server`；完整打包还生成
`target/zed-remote-server-macos-aarch64.gz`。

## 补全 DMG 许可协议

如果脚本已经生成 DMG，但最后报告 `dmg-license: command not found`，npm 可能已安装该命令，但其全局 `bin` 目录不在当前 `PATH`。可执行：

```sh
"$(npm prefix -g)/bin/dmg-license" \
  script/terms/terms.json \
  target/aarch64-apple-darwin/release/Zed-aarch64.dmg
```

## 验证产物

验证应用签名：

```sh
codesign --verify --deep --strict --verbose=2 \
  target/aarch64-apple-darwin/release/dmg/Zed.app
```

验证应用内二进制的架构：

```sh
file \
  target/aarch64-apple-darwin/release/dmg/Zed.app/Contents/MacOS/zed \
  target/aarch64-apple-darwin/release/dmg/Zed.app/Contents/MacOS/cli \
  target/aarch64-apple-darwin/release/dmg/Zed.app/Contents/MacOS/git
```

读取应用版本：

```sh
/usr/libexec/PlistBuddy \
  -c 'Print :CFBundleShortVersionString' \
  target/aarch64-apple-darwin/release/dmg/Zed.app/Contents/Info.plist
```

检查 DMG 格式及许可协议：

```sh
hdiutil imageinfo \
  target/aarch64-apple-darwin/release/Zed-aarch64.dmg \
  | rg 'Format Description|Total Bytes|Software License Agreement'
```

## 实验性复用官方 remote-server

main 分支的客户端版本可能尚未发布，因而官方没有对应的 Linux remote-server。
`script/build-mac.sh` 默认通过 GitHub Releases API 查询最新官方 stable 版本，
确认该发布包含 Linux remote-server，并将版本写入客户端默认值。自动查询需要 `curl`
和 `jq`；查询失败会停止构建。

可以显式固定官方 stable 服务端版本，跳过查询，而不改变客户端自己的版本号：

```sh
ZED_REMOTE_SERVER_VERSION=1.20.2 ./script/build-mac.sh --channel stable
```

构建时显式设置为空字符串可禁用自动查询和版本覆盖：

```sh
ZED_REMOTE_SERVER_VERSION= ./script/build-mac.sh
```

构建时设置的值会成为客户端默认值，因此从 Finder 启动也生效。`binaries` 模式同样支持，
但只更新二进制，不更新 `.app` 或 DMG。编译和打包时应使用相同的值，以复用构建缓存。

也可以在彻底退出所有 Zed 进程后，仅对这次启动覆盖版本：

```sh
ZED_REMOTE_SERVER_VERSION=1.20.2 /Applications/Zed.app/Contents/MacOS/zed
```

运行时环境变量优先于构建时默认值。显式设置为空字符串可禁用构建时的覆盖：

```sh
ZED_REMOTE_SERVER_VERSION= /Applications/Zed.app/Contents/MacOS/zed
```

该选项只接受精确的 stable 版本号，不接受 `latest`、预发布版本或构建元数据。
它同时作用于 SSH、WSL 和容器连接。以 `1.20.2` 为例，下载查询使用 stable / 1.20.2，
远端安装路径使用 `.zed_server/zed-remote-server-stable-1.20.2`，本地下载缓存沿用所选
官方产物的版本；Windows 远端文件名另带 `.exe`。
已有远端文件若报告不同版本，会重新获取指定产物。显式覆盖优先于开发模式自动编译和
`ZED_COPY_REMOTE_SERVER`，避免官方产物被本地构建覆盖。

版本选择在首次连接时固定，同一进程内不切换；更换覆盖值后需退出并重新启动 Zed。
未配置覆盖时保留原有行为。不要通过改名把其他版本伪装成当前客户端版本。

**这不保证 main 与官方服务端协议兼容。** 应验证连接、文件读写、终端和 Agent 会话；
失败时会显示连接错误供重试或取消，不会自动退回其他服务端版本。不同版本不兼容时，
仍需使用与客户端源码匹配的服务端。

## 签名说明

如果没有配置 `MACOS_CERTIFICATE`、`APPLE_NOTARIZATION_KEY` 等 Apple 发布凭据，
脚本使用 `MACOS_SIGNING_KEY` 指定的本地签名身份；未指定时使用 ad-hoc 临时签名。
该方式不会执行 Apple 公证，通用链接等依赖完整 entitlement 的功能可能不可用。

### 使用固定证书签名

每次重新编译会改变 ad-hoc 签名的应用身份，替换版本后可能再次弹出钥匙串访问请求。
使用固定证书签名，可以在更新应用时保持签名身份一致。

查询钥匙串中有效的代码签名身份：

```sh
security find-identity -v -p codesigning
```

本机当前查询结果为：

```text
1) 73C71571A1E993B2B628A5F4998CC201DB8336FA "Apple Development: cym8677@gmail.com (J8QP4MUX59)"
```

使用该证书的指纹构建并签名 stable 应用：

```sh
MACOS_SIGNING_KEY=73C71571A1E993B2B628A5F4998CC201DB8336FA \
  ./script/build-mac.sh --channel stable
```

在其他机器上或证书更新后，使用查询结果中的有效指纹。
`MACOS_SIGNING_KEY` 指定的是钥匙串中已有的证书及其私钥身份，不是文件路径或密码。
当前脚本未对该变量展开加引号，因此使用无空格的指纹，避免证书全名被拆分为多个参数。
每次构建使用相同身份；`binaries` 模式仅编译二进制，不执行应用签名。

构建完成后检查应用签名：

```sh
codesign -dv --verbose=4 \
  target/aarch64-apple-darwin/release/dmg/Zed.app

codesign --verify --deep --strict \
  target/aarch64-apple-darwin/release/dmg/Zed.app
```

签名信息应包含证书的 `Authority=`，而不是 `Signature=adhoc`。
从临时签名首次切换为证书签名后，仍可能需要重新授权一次钥匙串。

参考：[Apple 代码签名说明](https://developer.apple.com/library/archive/documentation/Security/Conceptual/CodeSigningGuide/Procedures/Procedures.html)。
