#!/usr/bin/env bash

set -euo pipefail

usage() {
    cat <<'USAGE'
用法: ./script/strip-remote-server.sh [选项] <remote_server 或 remote_server.gz>

在 macOS 或 Linux 上，为其他平台编译的 Linux remote_server 去掉调试信息和局部符号，
并生成 gzip 压缩包。处理方式与官方发布包和 build-linux-remote-server.sh 一致。

选项:
  --target TRIPLE    期望的目标三元组，与文件不符时报错；省略时根据文件自动识别
                     支持 x86_64 / aarch64 的 unknown-linux-gnu 和 unknown-linux-musl
  --output-dir DIR   输出文件夹，默认 <仓库>/tmp/remote-server-dist/<目标三元组>
  --objcopy PATH     指定 objcopy，默认自动查找 LLVM objcopy
  -h, --help         显示帮助

产物: <输出文件夹>/remote_server 和 <输出文件夹>/remote_server.gz
输入可以是原始二进制或 .gz，不会修改输入文件。
USAGE
}

fail() {
    echo "$*" >&2
    exit 2
}

target_triple=""
output_dir=""
objcopy=""
input=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --target|--output-dir|--objcopy)
            [[ $# -ge 2 && -n "$2" && "$2" != -* ]] || fail "$1 缺少参数"
            case "$1" in
                --target) target_triple="$2" ;;
                --output-dir) output_dir="$2" ;;
                --objcopy) objcopy="$2" ;;
            esac
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        -*) fail "未知参数: $1；使用 --help 查看用法" ;;
        *)
            [[ -z "$input" ]] || fail "只能指定一个输入文件"
            input="$1"
            shift
            ;;
    esac
done

[[ -n "$input" ]] || fail "缺少输入文件；使用 --help 查看用法"
[[ -f "$input" ]] || fail "输入文件不存在: $input"
case "$target_triple" in
    ""|x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu|x86_64-unknown-linux-musl|aarch64-unknown-linux-musl) ;;
    *) fail "不支持的目标: $target_triple" ;;
esac
command -v gzip >/dev/null 2>&1 || fail "缺少 gzip"
command -v od >/dev/null 2>&1 || fail "缺少 od"

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# 打印文件开头的若干字节，十六进制、以空格分隔。
header_bytes() {
    od -An -tx1 -N "$2" "$1" | tr -s ' \n' ' ' | sed 's/^ //; s/ $//'
}

# 根据 ELF 头识别架构，根据 file 的输出识别 libc。
detect_target() {
    local binary="$1" bytes arch libc description
    read -r -a bytes <<<"$(header_bytes "$binary" 20)"
    [[ "${#bytes[@]}" -ge 20 && "${bytes[0]}${bytes[1]}${bytes[2]}${bytes[3]}" == 7f454c46 ]] ||
        fail "不是 ELF 文件，不是 Linux remote_server: $input"
    [[ "${bytes[4]}" == 02 && "${bytes[5]}" == 01 ]] || fail "只支持 64 位小端 ELF: $input"
    case "${bytes[18]}${bytes[19]}" in
        3e00) arch=x86_64 ;;
        b700) arch=aarch64 ;;
        *) fail "不支持的 ELF 架构（e_machine=${bytes[19]}${bytes[18]}）: $input" ;;
    esac

    libc=unknown
    if command -v file >/dev/null 2>&1; then
        description=$(file -b "$binary")
        case "$description" in
            *ld-musl*) libc=musl ;;
            *ld-linux*) libc=gnu ;;
            *"statically linked"*|*"static-pie linked"*) libc=musl ;;
        esac
    fi
    echo "$arch $libc"
}

# rustup 的 rust-objcopy 可能因缺少 libLLVM 无法运行，只选能运行的候选。
runs() {
    "$1" --version >/dev/null 2>&1
}

find_objcopy() {
    local candidate
    # 优先使用 LLVM objcopy：macOS 自带的 strip 只能处理 Mach-O，
    # 较旧的 GNU objcopy 不认识新版 LLVM 生成的 CREL 节。
    for candidate in llvm-objcopy $(compgen -c llvm-objcopy- | sort -t- -k3 -rn | uniq); do
        if command -v "$candidate" >/dev/null 2>&1 && runs "$candidate"; then
            command -v "$candidate"
            return
        fi
    done
    for candidate in /opt/homebrew/opt/llvm/bin/llvm-objcopy /usr/local/opt/llvm/bin/llvm-objcopy; do
        if [[ -x "$candidate" ]] && runs "$candidate"; then
            echo "$candidate"
            return
        fi
    done
    if command -v rustc >/dev/null 2>&1; then
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
    fi
    for candidate in gobjcopy objcopy; do
        if command -v "$candidate" >/dev/null 2>&1 && runs "$candidate"; then
            echo "未找到 LLVM objcopy，改用 GNU objcopy: $candidate" >&2
            command -v "$candidate"
            return
        fi
    done
    return 1
}

if [[ -n "$objcopy" ]]; then
    if [[ ! -x "$objcopy" ]]; then
        resolved=$(command -v "$objcopy") || fail "找不到 objcopy: $objcopy"
        objcopy="$resolved"
    fi
else
    objcopy=$(find_objcopy) ||
        fail "缺少 LLVM objcopy；请安装 Rust 工具链，或执行 brew install llvm，或用 --objcopy 指定"
fi

work_dir=$(mktemp -d "${TMPDIR:-/tmp}/strip-remote-server.XXXXXX")
trap 'rm -rf "$work_dir"' EXIT

binary="$input"
if [[ "$(header_bytes "$input" 2)" == "1f 8b" ]]; then
    binary="${work_dir}/remote_server"
    gzip -dc "$input" > "$binary" || fail "解压失败: $input"
fi

detected=$(detect_target "$binary")
read -r detected_arch detected_libc <<<"$detected"
if [[ -n "$target_triple" ]]; then
    expected_arch="${target_triple%%-*}"
    expected_libc="${target_triple##*-}"
    [[ "$detected_arch" == "$expected_arch" ]] ||
        fail "文件架构是 ${detected_arch}，与指定的 ${target_triple} 不符"
    if [[ "$detected_libc" == unknown ]]; then
        echo "无法识别 libc，按指定的 ${target_triple} 处理" >&2
    elif [[ "$detected_libc" != "$expected_libc" ]]; then
        fail "文件是 ${detected_libc} 版本，与指定的 ${target_triple} 不符"
    fi
else
    [[ "$detected_libc" != unknown ]] ||
        fail "无法识别文件使用的 libc；请用 --target 指定目标三元组"
    target_triple="${detected_arch}-unknown-linux-${detected_libc}"
fi

if [[ -z "$output_dir" ]]; then
    output_dir="${repository_root}/tmp/remote-server-dist/${target_triple}"
fi
mkdir -p "$output_dir"
stripped_path="${output_dir}/remote_server"

echo "目标 ${target_triple}，使用 ${objcopy}"
"$objcopy" --strip-debug --discard-all "$binary" "${work_dir}/stripped" ||
    fail "去掉调试信息失败；GNU objcopy 版本过旧时请改用 LLVM objcopy"
detected=$(detect_target "${work_dir}/stripped")
read -r stripped_arch _ <<<"$detected"
[[ "$stripped_arch" == "$detected_arch" ]] || fail "处理后的文件架构异常"
chmod +x "${work_dir}/stripped"
gzip -9 -n -c "${work_dir}/stripped" > "${work_dir}/stripped.gz"

# 先在临时文件夹生成，成功后再替换，避免失败时留下不完整的产物。
mv -f "${work_dir}/stripped" "$stripped_path"
mv -f "${work_dir}/stripped.gz" "${stripped_path}.gz"

echo "输入: ${input}"
ls -lh "$input"
echo "产物:"
ls -lh "$stripped_path" "${stripped_path}.gz"
echo "可将 copy_remote_server 或 ZED_COPY_REMOTE_SERVER 指向以上任一文件。"
echo "该二进制无法在 macOS 上运行；上传后客户端会在远端执行 version 校验。"
