#!/usr/bin/env bash
# 发版：按日期取号，改工作区版本、更新 Cargo.lock、提交、打 tag。不推送，最后打印推送命令。
# 取号规则见 docs/release.md 的「版本号」。
#
#   scripts/release.sh             正式版：今天有进行中的 rc 就去掉 -rc.N 发它，否则取今天的新号
#   scripts/release.sh --rc        预发布：今天有进行中的 rc 就 rc.N+1，否则今天的新号加 -rc.1
#
# 日期一律是打 tag 那天：前几天没发完的 rc 不再接着用旧日期，取今天的新号。
#   scripts/release.sh --dry-run   只算出版本号，什么都不改（可与 --rc 同用）
#
# Cargo.toml 的版本平时就是最近发过的那个 tag，脚本靠它判断有没有进行中的 rc。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

die() { echo "✗ $*" >&2; exit 1; }

rc=0 dry_run=0
for arg in "$@"; do
  case "$arg" in
    --rc) rc=1 ;;
    --dry-run) dry_run=1 ;;
    *) die "未知参数：${arg}（可用 --rc、--dry-run）" ;;
  esac
done

workspace_version() {
  awk -F'"' '/^\[workspace.package\]/{inside=1; next} /^\[/{inside=0} inside && /^version *=/{print $2; exit}' Cargo.toml
}

# 比较 x.y.z 三段整数：$1 > $2 返回 0。
core_gt() {
  local a b i
  IFS=. read -r -a a <<<"$1"
  IFS=. read -r -a b <<<"$2"
  for i in 0 1 2; do
    ((a[i] > b[i])) && return 0
    ((a[i] < b[i])) && return 1
  done
  return 1
}

# tag 以 GitHub 上的为准：找指向 smelt-ai/smelt 的远端。
remote="$(git remote -v | awk '$2 ~ /github\.com[:\/]smelt-ai\/smelt(\.git)?$/ && $3 == "(push)" {print $1; exit}')"
[[ -n "$remote" ]] || die "没有指向 github.com/smelt-ai/smelt 的远端"

[[ "$(git rev-parse --abbrev-ref HEAD)" == main ]] || die "只在 main 上发版"
# 只提交 Cargo.toml 和 Cargo.lock，别的未提交改动不进这次提交。
git diff --quiet HEAD -- Cargo.toml Cargo.lock || die "Cargo.toml 或 Cargo.lock 有未提交的改动，先处理掉"
git fetch --quiet --tags "$remote" main
git merge-base --is-ancestor "$remote/main" HEAD || die "本地 main 落后于 $remote/main，先 pull"

current="$(workspace_version)"
[[ -n "$current" ]] || die "读不到工作区版本号"
git rev-parse -q --verify "refs/tags/v$current" >/dev/null \
  || die "Cargo.toml 的 $current 还没有对应的 tag v${current}；版本号应只由本脚本改"

today="$(date +%Y).$((10#$(date +%m)))$(date +%d)"

# 今天的新号：当天已有的号（含 rc）里最大的 N 加一，没有就从 0 起。
next_today() {
  local n=-1 tag rest
  while read -r tag; do
    rest="${tag#"v$today."}"
    rest="${rest%%-*}"
    if [[ "$rest" =~ ^[0-9]+$ ]] && ((rest > n)); then n=$rest; fi
  done < <(git tag --list "v$today.*")
  echo "$today.$((n + 1))"
}

if [[ "$current" == *-rc.* && "${current%%-*}" == "$today".* ]]; then
  base="${current%%-*}"
  if ((rc)); then
    version="$base-rc.$((${current##*-rc.} + 1))"
  else
    version="$base"
  fi
else
  base="$(next_today)"
  core_gt "$base" "$current" || die "算出的 $base 不比当前的 $current 新，检查系统日期"
  version="$base"
  if ((rc)); then version="$base-rc.1"; fi
fi

git rev-parse -q --verify "refs/tags/v$version" >/dev/null && die "tag v$version 已存在"
echo "▶ $current → $version"
if ((dry_run)); then exit 0; fi

tmp="$(mktemp)"
awk -v version="$version" '
  /^\[workspace.package\]/ { inside = 1 }
  /^\[/ && !/^\[workspace.package\]/ { inside = 0 }
  inside && !done && /^version *=/ { print "version = \"" version "\""; done = 1; next }
  { print }
' Cargo.toml >"$tmp"
mv "$tmp" Cargo.toml
[[ "$(workspace_version)" == "$version" ]] || die "改 Cargo.toml 失败"

# 只改工作区成员的版本行，不动依赖；Linux 打包用 --locked，锁文件必须跟上。
cargo update --workspace --quiet
if git diff -U0 Cargo.lock | grep -E '^[+-][^+-]' | grep -vqE '^[+-]version = '; then
  git diff Cargo.lock >&2
  git checkout -- Cargo.toml Cargo.lock
  die "Cargo.lock 改了版本行以外的内容（多半是有成员的 Cargo.toml 改了依赖还没提交），已还原"
fi

git commit --quiet -m "chore(release): v$version" -- Cargo.toml Cargo.lock
git tag -a "v$version" -m "v$version"
echo "✓ 已提交并打 tag v${version}。确认无误后推送："
echo "  git push --atomic $remote main v$version"
