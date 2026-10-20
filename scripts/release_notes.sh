#!/usr/bin/env bash
# 生成发布说明正文，写到 stdout。
#
#   scripts/release_notes.sh <tag> [上一个已发布的 tag]
#
# 为什么不用 `gh release create --generate-notes`：那个只汇总「合并进来的 PR」。
# 本项目直接往 main 推、不开 PR，于是 body 里只剩一行 Full Changelog ——
# v0.2.0-alpha.3 的发布页就是这样，等于没有变更说明。这里改成读 `git log`。
#
# 分组规则（只看 commit subject 的 Conventional Commit 前缀；没有前缀时只看
# 首个动词，不做别的猜测——猜错会把修复塞进「新增」里，比不分组更糟）：
#
#   feat / Add*            -> Features
#   fix / Fix*             -> Fixes
#   docs / Document*       -> Docs
#   perf                   -> Performance
#   refactor / test        -> Internal
#   build / ci / chore     -> Maintenance（chore(release) 的版本提交直接丢弃）
#   其它                   -> Other
#
# 另外按 RELEASING.md 的要求，如果 src/format.rs 里 MAGIC 或 VERSION 的定义被
# 改动，会在最前面插入「需要重建索引」的提示。这条漏掉，用户就会遇到加载失败
# 而没有任何解释。
set -euo pipefail

TAG="${1:?用法: scripts/release_notes.sh <tag> [上一个已发布的 tag]}"
PREV="${2:-}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

git rev-parse -q --verify "refs/tags/$TAG" >/dev/null || {
  echo "错误: 本地没有 tag $TAG" >&2
  exit 1
}
if [[ -n "$PREV" ]]; then
  git rev-parse -q --verify "refs/tags/$PREV" >/dev/null || {
    echo "错误: 本地没有 tag $PREV" >&2
    exit 1
  }
  RANGE="$PREV..$TAG"
else
  RANGE="$TAG"
fi

REPO_SLUG="${GITHUB_REPOSITORY:-}"
if [[ -z "$REPO_SLUG" ]]; then
  REPO_SLUG="$(git remote get-url origin \
    | sed -E 's#^[^:]+@[^:]+:##; s#^https?://[^/]+/##; s#\.git$##')"
fi

TMP="$(mktemp)"
# 清理失败不该让脚本以非零退出——说明生成成功与否和删临时文件没关系。
trap 'rm -f "$TMP" 2>/dev/null || true' EXIT

# ---------------------------------------------------------------- 索引格式
# 只看真正的常量定义行（`const MAGIC:` / `pub const VERSION:`），
# 不匹配文档里顺口提到这两个名字的注释，否则会误报。
INDEX_CHANGED=0
if [[ -n "$PREV" ]]; then
  if git diff --unified=0 "$PREV..$TAG" -- src/format.rs \
     | grep -qE '^[+-][[:space:]]*(pub([[:space:]]+\(crate\))?[[:space:]]+)?const[[:space:]]+(MAGIC|VERSION)[[:space:]]*:'; then
    INDEX_CHANGED=1
  fi
fi

# ---------------------------------------------------------------- 分组
git log --no-merges --reverse --format='%h%x09%s' "$RANGE" \
  | while IFS=$'\t' read -r hash subject; do
      [[ -n "$subject" ]] || continue

      if [[ "$subject" == *": "* ]]; then
        kind="${subject%%: *}"        # feat(ui) -> feat
        kind="${kind%%(*}"
        msg="${subject#*: }"
      else
        kind=""
        msg="$subject"
      fi

      case "$kind" in
        chore) continue ;;            # chore(release): vX —— 版本提交，不说也罢
        feat)  bucket=FEATURES ;;
        fix)   bucket=FIXES ;;
        docs)  bucket=DOCS ;;
        perf)  bucket=PERFORMANCE ;;
        refactor|test) bucket=INTERNAL ;;
        build|ci)      bucket=MAINTENANCE ;;
        "")
          case "$msg" in
            Add*|Support*|Introduce*) bucket=FEATURES ;;
            Fix*|Correct*|Repair*)    bucket=FIXES ;;
            Document*|README*)        bucket=DOCS ;;
            *)                        bucket=OTHER ;;
          esac
          ;;
        *) bucket=OTHER ;;
      esac

      printf '%s\t- %s (`%s`)\n' "$bucket" "$msg" "$hash"
    done > "$TMP"

# ---------------------------------------------------------------- 输出
if [[ "$INDEX_CHANGED" == 1 ]]; then
  cat <<'EOF'
> [!WARNING]
> **Index format changed — re-index required.**
> Existing `.fst` files from the previous version cannot be reused; delete them
> and let mftfind rebuild. `src/format.rs` changed `MAGIC` or `VERSION`.

EOF
fi

emit() { # $1=分组键 $2=标题
  local lines
  lines="$(grep -E "^$1"$'\t' "$TMP" | cut -f2- || true)"
  [[ -n "$lines" ]] && printf '### %s\n\n%s\n\n' "$2" "$lines"
  return 0
}

if [[ -n "$PREV" ]]; then
  printf '### Changes since %s\n\n' "$PREV"
else
  printf '### Changes\n\n'
fi

emit FEATURES     "Features"
emit FIXES        "Fixes"
emit PERFORMANCE  "Performance"
emit DOCS         "Docs"
emit INTERNAL     "Internal"
emit MAINTENANCE  "Build & CI"
emit OTHER        "Other"

if [[ ! -s "$TMP" ]]; then
  printf '_No code changes since the previous release._\n\n'
fi

if [[ -n "$PREV" ]]; then
  printf '**Full Changelog**: https://github.com/%s/compare/%s...%s\n\n' \
    "$REPO_SLUG" "$PREV" "$TAG"
fi

printf 'The executable is unsigned, so Windows SmartScreen will warn on first launch.\n'
