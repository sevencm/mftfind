#!/usr/bin/env bash
# Cut a release: validate the version, bump Cargo.toml, refresh Cargo.lock,
# commit, and create the annotated tag.
#
#   ./scripts/release.sh 0.2.0-alpha.1
#   ./scripts/release.sh 0.2.0-alpha.1 --push    # also push, triggering CI
#
# Pushing the tag is what starts .github/workflows/release.yml.
# The version rules are defined in RELEASING.md.
set -euo pipefail

RULE='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-(alpha|beta|rc)\.(0|[1-9][0-9]*))?$'
BRANCH_EXPECTED="${RELEASE_BRANCH:-main}"

die() { echo "错误: $*" >&2; exit 1; }

usage() {
  cat >&2 <<'EOF'
用法: scripts/release.sh <版本号> [--push]

  版本号必须符合 RELEASING.md 的规则：
    0.1.0              正式版
    0.2.0-alpha.1      内测
    0.2.0-beta.1       公测
    0.2.0-rc.1         候选发布

  --push   提交并推送 commit 与 tag（会触发 GitHub 发布流程）
           不加则只在本地完成，并打印待执行的推送命令
EOF
  exit 2
}

VERSION="${1:-}"
PUSH="${2:-}"
[[ -n "$VERSION" ]] || usage
[[ "$VERSION" =~ $RULE ]] || die "版本号 '$VERSION' 不符合规则，见 RELEASING.md（示例: 0.2.0-alpha.1）"
[[ -z "$PUSH" || "$PUSH" == "--push" ]] || usage

TAG="v$VERSION"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# ---------- 前置检查 ----------
[[ -f Cargo.toml ]] || die "在 $ROOT 下找不到 Cargo.toml"
[[ -z "$(git status --porcelain)" ]] || die "工作区不干净，请先提交或 stash 改动"

BRANCH="$(git rev-parse --abbrev-ref HEAD)"
[[ "$BRANCH" == "$BRANCH_EXPECTED" ]] || die "当前分支是 $BRANCH，发布应在 $BRANCH_EXPECTED 上进行"

if git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
  die "tag $TAG 已存在。不要复用或移动 tag，请改用下一个序数（如 alpha.2）"
fi

# ---------- 解析当前版本 ----------
read_pkg_version() {
  awk '
    /^\[package\]/ { in_pkg = 1; next }
    /^\[/          { in_pkg = 0 }
    in_pkg && /^version[[:space:]]*=/ {
      gsub(/[^0-9A-Za-z.+_-]/, "", $3)
      print $3
      exit
    }
  ' Cargo.toml
}

CURRENT="$(read_pkg_version)"
[[ -n "$CURRENT" ]] || die "无法从 Cargo.toml 解析 package.version"
[[ "$CURRENT" != "$VERSION" ]] || die "Cargo.toml 已经是 $VERSION，无需再改"

# ---------- 判定通道 ----------
CHANNEL="stable"
if [[ "$VERSION" == *-* ]]; then
  SUFFIX="${VERSION#*-}"
  CHANNEL="${SUFFIX%.*}"
fi

# ---------- 改写 Cargo.toml ----------
# MSYS 下的 awk / sed 在读取时会吞掉 \r（本机 core.autocrlf=true，工作区是 CRLF）。
# 如果直接 print，整个文件会被改写成 LF，提交时变成「每一行都改了」的假 diff。
# 所以先用 od 从原始字节判断行尾，再在写出时显式还原。
if head -c 512 Cargo.toml | od -c | grep -q '\\r'; then
  CRLF=1
else
  CRLF=0
fi

awk -v v="$VERSION" -v crlf="$CRLF" '
  { sub(/\r$/, "") }
  /^\[package\]/ { in_pkg = 1; printf "%s%s", $0, (crlf ? "\r\n" : "\n"); next }
  /^\[/          { in_pkg = 0 }
  in_pkg && !done && /^version[[:space:]]*=/ {
    printf "version = \"%s\"%s", v, (crlf ? "\r\n" : "\n")
    done = 1
    next
  }
  { printf "%s%s", $0, (crlf ? "\r\n" : "\n") }
' Cargo.toml > Cargo.toml.new
mv Cargo.toml.new Cargo.toml

AFTER="$(read_pkg_version)"
[[ "$AFTER" == "$VERSION" ]] || die "改写 Cargo.toml 失败（当前为 $AFTER）"
echo "  Cargo.toml: $CURRENT -> $VERSION"

# ---------- 刷新 Cargo.lock ----------
# CI 用 --locked 构建，lock 里的版本号必须同步，否则发布流程会失败。
# 注意 ~/.cargo/bin 下的 cargo 是 rustup 代理外壳，在本沙箱会静默失效，
# 因此优先使用工具链本体（与 build.sh 的策略一致）。
TOOLCHAIN_CARGO="/c/Users/admin/.rustup/toolchains/stable-x86_64-pc-windows-gnu/bin/cargo.exe"
if [[ -n "${CARGO:-}" ]]; then
  CARGO_BIN="$CARGO"
elif [[ -x "$TOOLCHAIN_CARGO" ]]; then
  CARGO_BIN="$TOOLCHAIN_CARGO"
else
  CARGO_BIN="cargo"
fi

echo "  刷新 Cargo.lock（$CARGO_BIN）"
"$CARGO_BIN" metadata --format-version 1 > /dev/null

if git diff --quiet -- Cargo.lock; then
  echo "  Cargo.lock 无需变更"
else
  echo "  Cargo.lock 已同步"
fi

# ---------- 提交 + 打 tag ----------
git add Cargo.toml Cargo.lock
git commit -q -m "chore(release): $TAG"
git tag -a "$TAG" -m "mftfind $TAG"

echo "  提交: $(git rev-parse --short HEAD)"
echo "  tag : $TAG（通道 $CHANNEL）"

# ---------- 推送 ----------
if [[ "$PUSH" == "--push" ]]; then
  git push origin "$BRANCH"
  git push origin "$TAG"
  echo
  echo "已推送。发布流程即将启动，可在仓库的 Actions 页查看进度。"
else
  echo
  echo "仅在本地完成。执行下面两条命令即可触发发布："
  echo "  git push origin $BRANCH"
  echo "  git push origin $TAG"
fi
