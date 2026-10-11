#!/usr/bin/env bash

set -euo pipefail

usage() {
    cat <<'USAGE'
用法: ./script/build-linux-remote-server.sh [选项]

在 Linux 上仅以 release 模式编译 remote_server，不构建客户端、CLI 或安装产物。

选项:
  --channel CHANNEL  dev / nightly / preview / stable，默认 stable
  --target TRIPLE    Linux Rust 目标三元组，默认 TARGET_TRIPLE 或 rustc host
  --jobs NUMBER      编译并发数，例如 --jobs 2；默认沿用 Cargo 配置
  -h, --help         显示帮助

支持 x86_64 / aarch64 的 GNU 和 musl 目标。
非原生目标需要自行安装对应 Rust target、C/C++ 工具链和链接器。
使用当前工作区源码；请确保与客户端源码和通道一致。
USAGE
}

fail() {
    echo "$*" >&2
    exit 2
}

channel=stable
target_triple="${TARGET_TRIPLE:-}"
jobs=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --channel|--target|--jobs)
            [[ $# -ge 2 && -n "$2" && "$2" != -* ]] || fail "$1 缺少参数"
            case "$1" in
                --channel) channel="$2" ;;
                --target) target_triple="$2" ;;
                --jobs) jobs="$2" ;;
            esac
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *) fail "未知参数: $1；使用 --help 查看用法" ;;
    esac
done

case "$channel" in
    dev|nightly|preview|stable) ;;
    *) fail "未知通道: $channel" ;;
esac
if [[ -n "$jobs" && ! "$jobs" =~ ^[1-9][0-9]*$ ]]; then
    fail "--jobs 必须是正整数"
fi

[[ "$(uname -s)" == Linux ]] || fail "此脚本仅在 Linux 上运行，不进行 macOS 交叉编译"
for tool in cargo rustc; do
    command -v "$tool" >/dev/null 2>&1 || fail "缺少 $tool；请先安装 Rust 工具链"
done

cd "$(dirname "${BASH_SOURCE[0]}")/.."

if [[ -z "$target_triple" ]]; then
    target_triple=$(rustc -vV | sed -n 's/^host: //p')
fi
case "$target_triple" in
    x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu|x86_64-unknown-linux-musl|aarch64-unknown-linux-musl) ;;
    *) fail "不支持的 Linux 目标: $target_triple" ;;
esac

mkdir -p tmp docs/tmp
export TMPDIR="$PWD/docs/tmp"
export TEMP="$TMPDIR"
export TMP="$TMPDIR"
export ZED_RELEASE_CHANNEL="$channel"
export ZED_REMOTE_SERVER_VERSION=""

target_dir="${CARGO_TARGET_DIR:-target}"
binary_path="${target_dir}/${target_triple}/release/remote_server"

build_arguments=(
    --config .cargo/bundle-config.toml
    build --release --locked
    --package remote_server --bin remote_server
    --target "$target_triple" --target-dir "$target_dir"
)
if [[ -n "$jobs" ]]; then
    build_arguments+=(--jobs "$jobs")
fi

echo "仅编译 Linux remote_server：目标 ${target_triple}，通道 ${channel}"
cargo "${build_arguments[@]}"

echo "构建完成，产物: ${binary_path}"
ls -lh "$binary_path"
echo "请在目标 Linux 上执行该产物的 version 和 --help，确认版本及 exec 子命令。"
