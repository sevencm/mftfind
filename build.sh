#!/usr/bin/env bash
# mftfind Rust 构建脚本
#
# 三个必须绕过/补齐的环境点（都是这台机器上实测出来的）：
#   1. ~/.cargo/bin 下的 rustc/cargo 是 rustup 代理外壳，在本沙箱里会静默失效（退出码 0 但无输出），
#      因此直接用工具链本体的绝对路径。
#   2. windows-targets / windows-result 等 crate 在 windows-gnu 目标下用 raw-dylib，
#      编译期需要 dlltool.exe 生成导入库；它随工具链提供，位于
#      lib/rustlib/x86_64-pc-windows-gnu/bin/self-contained/，默认不在 PATH 上。
#      注意：往 PATH 里加目录必须用 POSIX 写法（/c/Users/...），
#      写成 C:/Users/... 会被 MSYS bash 按冒号切成两段无效路径，表现为
#      "error calling dlltool 'dlltool.exe': program not found"。
#   3. 依赖源走国内镜像（配置在 ~/.cargo/config.toml），否则 356 个界面依赖要拉很久。
set -euo pipefail

TOOLCHAIN="/c/Users/admin/.rustup/toolchains/stable-x86_64-pc-windows-gnu/bin"
export RUSTC="$TOOLCHAIN/rustc.exe"
export CARGO="$TOOLCHAIN/cargo.exe"
export CARGO_HOME="C:/Users/admin/.cargo"
export RUSTUP_HOME="C:/Users/admin/.rustup"
# 关键：把工具链自带的 dlltool.exe / ld.exe 所在目录加进 PATH（POSIX 形式！）
export PATH="/c/Users/admin/.rustup/toolchains/stable-x86_64-pc-windows-gnu/lib/rustlib/x86_64-pc-windows-gnu/bin/self-contained:$PATH"

cd "$(dirname "$0")"

if [ "${1:-}" = "--debug" ]; then
  shift
  "$CARGO" build "$@"
else
  "$CARGO" build --release "$@"
fi

echo "构建完成: $(pwd)/target/release/mftfind.exe"
