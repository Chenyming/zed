#!/usr/bin/env bash

set -euo pipefail

usage() {
    cat <<'USAGE'
用法: ./script/build-linux-remote-server.sh [选项]

在 Linux 上仅以 release 模式编译 remote_server，不构建客户端、CLI 或安装产物。
编译后去掉调试信息并生成 gzip 压缩包，与官方发布包的处理方式一致。

选项:
  --channel CHANNEL  dev / nightly / preview / stable，默认 stable
  --target TRIPLE    Linux Rust 目标三元组，默认 TARGET_TRIPLE 或 rustc host
  --jobs NUMBER      编译并发数，例如 --jobs 2；默认沿用 Cargo 配置
  --no-strip         只编译，不生成去掉调试信息的产物和压缩包
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
strip_binary=true

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
        --no-strip)
            strip_binary=false
            shift
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

echo "构建完成，未去掉调试信息的产物: ${binary_path}"
ls -lh "$binary_path"

if [[ "$strip_binary" == false ]]; then
    echo "请在目标 Linux 上执行该产物的 version 和 --help，确认版本及 exec 子命令。"
    exit 0
fi

# 优先使用 llvm-objcopy：较旧发行版的 GNU objcopy 不认识新版 LLVM 生成的 CREL 节。
# rustup 的 rust-objcopy 可能因缺少 libLLVM 无法运行，只选能运行的候选。
runs() {
    "$1" --version >/dev/null 2>&1
}

find_objcopy() {
    local candidate
    for candidate in llvm-objcopy $(compgen -c llvm-objcopy- | sort -t- -k3 -rn | uniq); do
        if command -v "$candidate" >/dev/null 2>&1 && runs "$candidate"; then
            command -v "$candidate"
            return
        fi
    done
    local host sysroot
    host=$(rustc -vV | sed -n 's/^host: //p')
    sysroot=$(rustc --print sysroot)
    # rustc 自带 rust-objcopy（LLVM objcopy）；llvm-tools 组件提供 llvm-objcopy。
    for candidate in rust-objcopy llvm-objcopy; do
        candidate="${sysroot}/lib/rustlib/${host}/bin/${candidate}"
        if [[ -x "$candidate" ]] && runs "$candidate"; then
            echo "$candidate"
            return
        fi
    done
    if command -v objcopy >/dev/null 2>&1; then
        echo "未找到 llvm-objcopy，改用 GNU objcopy" >&2
        command -v objcopy
        return
    fi
    return 1
}

objcopy=$(find_objcopy) || fail "缺少 llvm-objcopy；请安装 llvm，或执行 rustup component add llvm-tools"
command -v gzip >/dev/null 2>&1 || fail "缺少 gzip"

output_dir="${target_dir}/remote-server-dist/${target_triple}"
stripped_path="${output_dir}/remote_server"
mkdir -p "$output_dir"
rm -f "$stripped_path" "${stripped_path}.gz"

"$objcopy" --strip-debug --discard-all "$binary_path" "$stripped_path" ||
    fail "去掉调试信息失败；GNU objcopy 版本过旧时请安装 llvm-objcopy"
gzip --best --no-name --stdout "$stripped_path" > "${stripped_path}.gz"

echo "去掉调试信息的产物: ${stripped_path}"
echo "压缩包: ${stripped_path}.gz"
ls -lh "$stripped_path" "${stripped_path}.gz"
echo "上传到 Mac 时可使用以上任一文件；未去掉调试信息的产物仅用于本地调试和符号化。"
echo "请在目标 Linux 上执行该产物的 version 和 --help，确认版本及 exec 子命令。"
