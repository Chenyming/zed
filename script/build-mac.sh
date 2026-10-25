#!/usr/bin/env bash
#
# 本地编译 / 打包 Zed macOS release,参考 docs/local-release-build-and-macos-bundle.zh-CN.md
#
# 用法:
#   script/build-mac.sh                     完整流程:release 二进制 + .app + DMG
#   script/build-mac.sh binaries            只编 release 二进制,跳过 bundle 和 DMG(日常迭代用)
#   script/build-mac.sh open                完整流程,结束后打开产物所在目录
#   script/build-mac.sh --channel stable    临时用 stable 渠道构建,结束后自动还原
#   script/build-mac.sh --with-remote-server 同时编译 macOS remote_server
#   script/build-mac.sh --with-copy-remote-server 启用客户端 ZED_COPY_REMOTE_SERVER 上传能力
#
# 渠道可选 dev / nightly / preview / stable,默认沿用 crates/zed/RELEASE_CHANNEL 当前的值。
#
# 环境变量:

#   MIN_FREE_GB   构建前要求的最小可用空间,默认 60
#   TARGET_TRIPLE 目标三元组,默认取 rustc 的 host
#   MACOS_SIGNING_KEY 默认使用固定本地证书 73C71571A1E993B2B628A5F4998CC201DB8336FA;设为 - 使用 ad-hoc 签名
#   ZED_REMOTE_SERVER_VERSION 默认查询最新官方 stable 版本;可指定版本,设为空则禁用覆盖

set -euo pipefail

cd "$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel)"

mode="full"
requested_channel=""
build_remote_server=false
copy_remote_server=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        full|binaries|open)
            mode="$1"
            shift
            ;;
        --channel)
            if [[ $# -lt 2 ]]; then
                echo "--channel 缺少参数" >&2
                exit 2
            fi
            requested_channel="$2"
            shift 2
            ;;
        --channel=*)
            requested_channel="${1#--channel=}"
            shift
            ;;
        --with-remote-server)
            build_remote_server=true
            shift
            ;;
        --with-copy-remote-server)
            copy_remote_server=true
            shift
            ;;
        *)
            echo "未知参数: $1" >&2
            echo "可选:full / binaries / open,--channel dev|nightly|preview|stable,--with-remote-server,--with-copy-remote-server" >&2
            exit 2
            ;;
    esac
done

channel_file="crates/zed/RELEASE_CHANNEL"
current_channel=$(tr -d '[:space:]' < "$channel_file")
channel="${requested_channel:-$current_channel}"

case "$channel" in
    dev|nightly|preview|stable) ;;
    *) echo "未知渠道: $channel(可选 dev / nightly / preview / stable)" >&2; exit 2 ;;
esac


export MACOS_SIGNING_KEY="${MACOS_SIGNING_KEY:-73C71571A1E993B2B628A5F4998CC201DB8336FA}"

TARGET_TRIPLE="${TARGET_TRIPLE:-$(rustc -vV | sed -n 's/^host: //p')}"
MIN_FREE_GB="${MIN_FREE_GB:-60}"

# 这个仓库的 target 目录很容易涨到上百 G,磁盘写满时 cargo 会以
# "sha1 file write error" 之类的形式失败,并且可能留下半写坏的工作区。
free_gb=$(df -g . | awk 'NR==2 {print $4}')
if [[ "$free_gb" -lt "$MIN_FREE_GB" ]]; then
    echo "可用空间只有 ${free_gb}G,低于要求的 ${MIN_FREE_GB}G。" >&2
    echo "清理建议:rm -rf target/${TARGET_TRIPLE}(只删打包树),或 cargo sweep --time 7" >&2
    exit 1
fi
echo "可用空间 ${free_gb}G,目标三元组 ${TARGET_TRIPLE},渠道 ${channel}"

if [[ "$copy_remote_server" == true ]]; then
    if [[ -n "${ZED_REMOTE_SERVER_VERSION:-}" ]]; then
        echo "--with-copy-remote-server 不能与非空 ZED_REMOTE_SERVER_VERSION 同时使用：版本覆盖会跳过本地上传。" >&2
        exit 2
    fi
    ZED_REMOTE_SERVER_VERSION=""
    echo "启用 ZED_COPY_REMOTE_SERVER 上传能力，禁用官方服务端版本覆盖和自动查询"
elif [[ -z "${ZED_REMOTE_SERVER_VERSION+x}" ]]; then
    if ! command -v jq >/dev/null 2>&1; then
        echo "自动查询 remote-server 版本需要 jq;请安装 jq 或显式设置 ZED_REMOTE_SERVER_VERSION。" >&2
        exit 1
    fi
    echo "查询最新官方 stable remote-server 版本"
    if ! ZED_REMOTE_SERVER_VERSION=$(
        curl --fail --silent --show-error --location \
            --connect-timeout 10 --max-time 30 --retry 2 \
            https://api.github.com/repos/zed-industries/zed/releases/latest |
            jq --exit-status --raw-output '
                select(.draft == false and .prerelease == false)
                | select(any(.assets[]; .name | startswith("zed-remote-server-linux-")))
                | .tag_name | strings | sub("^v"; "")
            '
    ); then
        echo "查询最新 remote-server 版本失败;请检查网络或显式设置 ZED_REMOTE_SERVER_VERSION 后重试。" >&2
        exit 1
    fi
fi

if [[ -n "${ZED_REMOTE_SERVER_VERSION:-}" ]]; then
    if [[ ! "$ZED_REMOTE_SERVER_VERSION" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
        echo "ZED_REMOTE_SERVER_VERSION 必须是精确的 stable 版本号(如 1.23.2),或为空以禁用覆盖。" >&2
        exit 1
    fi
    echo "固定官方 stable remote-server 为 ${ZED_REMOTE_SERVER_VERSION}(与自编译客户端的协议兼容性需自行验证)"
fi
export ZED_REMOTE_SERVER_VERSION

# cargo-bundle 在部分终端下会因为颜色能力探测失败抛 Term(ColorOutOfRange)。
export TERM=xterm-256color
export CARGO_TERM_COLOR=never

# 把临时文件放进仓库内的 docs/tmp,避免 /tmp 所在卷被链接阶段的大文件挤爆。
mkdir -p tmp docs/tmp
export TMPDIR="$PWD/docs/tmp"
export TEMP="$TMPDIR"
export TMP="$TMPDIR"

channel_backup=""
started_at=$SECONDS

cleanup() {
    local status=$?
    if [[ -n "$channel_backup" && -f "$channel_backup" ]]; then
        mv "$channel_backup" "$channel_file"
        echo "已还原 ${channel_file} 为 ${current_channel}"
    fi
    echo "耗时 $(( (SECONDS - started_at) / 60 ))m$(( (SECONDS - started_at) % 60 ))s"
    return $status
}
trap cleanup EXIT INT TERM

if [[ "$channel" != "$current_channel" ]]; then
    if [[ "$channel" == "stable" ]]; then
        echo "警告:stable 渠道的 bundle id 是 dev.zed.Zed,与官方版 Zed 相同。" >&2
        echo "      装进 /Applications 会覆盖官方版,且运行时共用数据库目录 db/0-stable。" >&2
    fi
    # RELEASE_CHANNEL 是版本库跟踪的文件,这里备份原件、退出时无条件还原,
    # 避免忘记恢复而误提交。
    channel_backup=$(mktemp "${TMPDIR}/RELEASE_CHANNEL.XXXXXX")
    cp "$channel_file" "$channel_backup"
    printf '%s\n' "$channel" > "$channel_file"
    echo "临时把 ${channel_file} 改为 ${channel}(结束后自动还原)"
fi

# 这三个变量 script/bundle-mac 也会导出,而且都会被编进二进制:
# ZED_RELEASE_CHANNEL 经 crates/release_channel/build.rs 决定应用名和数据库
# 目录,ZED_BUNDLE 经 crates/language/build.rs 决定是否使用内置 git。
# binaries 模式必须原样复现,否则 fingerprint 和完整打包不一致,产物无法复用。
export ZED_RELEASE_CHANNEL="$channel"
export ZED_BUNDLE=true
export CXXFLAGS="-stdlib=libc++"

if [[ "$mode" == "binaries" ]]; then
    # 这两条必须和 script/bundle-mac 里的调用逐字一致(尤其是
    # --config .cargo/bundle-config.toml),否则 rustflags 不同会在同一个
    # 目录里生成第二套 fingerprint,完整打包时等于从头再编一遍。
    echo "编译 zed / cli"
    set -- --config .cargo/bundle-config.toml build --release \
        --package zed --package cli --target "$TARGET_TRIPLE"
    if [[ "$copy_remote_server" == true ]]; then
        set -- "$@" --features remote/build-remote-server-binary
    fi
    cargo "$@"

    if [[ "$build_remote_server" == true ]]; then
        # remote_server 单独一次调用,避免其他 crate 的 feature 统一影响它
        # 依赖的动态库。
        echo "编译 remote_server"
        cargo --config .cargo/bundle-config.toml build --release \
            --package remote_server --target "$TARGET_TRIPLE"
    fi

    echo "二进制产物:"
    set -- "target/${TARGET_TRIPLE}/release/zed" "target/${TARGET_TRIPLE}/release/cli"
    if [[ "$build_remote_server" == true ]]; then
        set -- "$@" "target/${TARGET_TRIPLE}/release/remote_server"
    fi
    ls -lh "$@"
    exit 0
fi

set -- "$TARGET_TRIPLE"
if [[ "$build_remote_server" == true ]]; then
    set -- -r "$@"
fi
if [[ "$copy_remote_server" == true ]]; then
    set -- -c "$@"
fi
if [[ "$mode" == "open" ]]; then
    set -- -o "$@"
fi
./script/bundle-mac "$@"

# app 的名字随渠道变("Zed Dev" / "Zed Preview" / "Zed Nightly" / "Zed"),
# 所以这里靠通配而不是写死名字。
app_path=$(ls -d "target/${TARGET_TRIPLE}/release/dmg/"*.app 2>/dev/null | head -1 || true)
dmg_path="target/${TARGET_TRIPLE}/release/Zed-$([[ "$TARGET_TRIPLE" == aarch64-* ]] && echo aarch64 || echo x86_64).dmg"

echo
echo "产物:"
if [[ -n "$app_path" && -d "$app_path" ]]; then
    du -sh "$app_path"
    /usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$app_path/Contents/Info.plist"
    /usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$app_path/Contents/Info.plist"
    codesign --verify --deep --strict "$app_path" && echo "签名校验通过"
else
    echo "未找到 .app,检查 bundle-mac 的输出" >&2
fi
[[ -f "$dmg_path" ]] && du -sh "$dmg_path"

# bundle-mac 在缺少 dmg-license 时会在最后一步报 command not found,
# 但 DMG 本身已经生成;这里补一次许可协议。
if [[ -f "$dmg_path" ]] && ! hdiutil imageinfo "$dmg_path" | grep -q 'Software License Agreement: true'; then
    dmg_license="$(npm prefix -g 2>/dev/null)/bin/dmg-license"
    if [[ -x "$dmg_license" ]]; then
        echo "补加 DMG 许可协议"
        "$dmg_license" script/terms/terms.json "$dmg_path"
    else
        echo "未找到 dmg-license,DMG 不含许可协议(不影响本机使用)"
    fi
fi
