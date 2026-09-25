#!/bin/sh
# 检查 docs/STATE_V2.md：每条规则 **V2-N** 都有测试名，编号连续，测试名不重复。
#
#   tests/state_v2_doc_check.sh [文档]          只查文档
#   tests/state_v2_doc_check.sh --impl [文档]   另外查 data-service 的测试函数都已写出（T3–T5 之后用）
#   tests/state_v2_doc_check.sh --self-test     用故意写坏的文档确认检查会失败
#
# 规则的写法：一行以 **V2-N** 开头；到下一条规则或下一个标题为止，
# 以「测试」开头的行里，`反引号` 中的名字就是测试名。标着 manager 的那一段是 manager 仓库的测试，--impl 不查。
# SPDX-License-Identifier: MIT
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)

# 输出「规则编号 测试名 仓库」，每行一个；文档有错时往 stderr 写原因并返回非 0。
extract() {
  awk '
    function fail(msg) { print FILENAME ":" NR ": " msg > "/dev/stderr"; bad = 1 }
    function close_rule() {
      if (rule != "" && ntests[rule] == 0) { print "规则 " rule " 没有测试名" > "/dev/stderr"; bad = 1 }
      rule = ""
    }
    /^#/ { close_rule(); next }
    /^\*\*V2-[0-9]+\*\*/ {
      close_rule()
      id = $0; sub(/^\*\*V2-/, "", id); sub(/\*\*.*/, "", id)
      if (seen[id]++) fail("规则 V2-" id " 重复")
      if (id + 0 != last + 1) fail("规则编号不连续：V2-" last " 之后是 V2-" id)
      last = id + 0; rule = "V2-" id; ntests[rule] = 0; nrules++
      next
    }
    rule != "" && /^测试/ {
      n = split($0, segs, "测试")
      for (s = 2; s <= n; s++) {
        repo = (segs[s] ~ /manager/) ? "manager" : "data-service"
        rest = segs[s]
        while (match(rest, /`[^`]*`/)) {
          name = substr(rest, RSTART + 1, RLENGTH - 2)
          rest = substr(rest, RSTART + RLENGTH)
          if (name !~ /^[a-z][a-z0-9_]*$/) { fail("测试名不是小写加下划线：" name); continue }
          if (name in owner) fail("测试名 " name " 同时出现在 " owner[name] " 和 " rule)
          owner[name] = rule
          ntests[rule]++
          print rule, name, repo
        }
      }
    }
    END {
      close_rule()
      if (nrules == 0) { print "没有找到任何 **V2-N** 规则" > "/dev/stderr"; bad = 1 }
      exit bad
    }
  ' "$1"
}

check() {
  doc=$1 impl=$2
  list=$(extract "$doc") || return 1
  if [ "$impl" = 1 ]; then
    missing=$(echo "$list" | while read -r rule name repo; do
      [ "$repo" = data-service ] || continue
      grep -rqE "fn ${name}[[:space:]]*\\(" "$ROOT/rust/src" "$ROOT/rust/tests" 2>/dev/null ||
        echo "$rule 的测试 $name 还没写"
    done)
    if [ -n "$missing" ]; then
      echo "$missing" >&2
      return 1
    fi
  fi
  rules=$(echo "$list" | awk '{print $1}' | sort -u | wc -l | tr -d ' ')
  tests=$(echo "$list" | wc -l | tr -d ' ')
  echo "ok：$rules 条规则，$tests 个测试名（$doc）"
}

self_test() {
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  good=$ROOT/docs/STATE_V2.md
  expect_fail() {
    if check "$tmp/$1.md" 0 >/dev/null 2>"$tmp/$1.err"; then
      echo "self-test 失败：$1 本该报错却通过了" >&2; exit 1
    fi
    echo "  $1：报错如期（$(head -n 1 "$tmp/$1.err")）"
  }
  check "$good" 0 >/dev/null || { echo "self-test 失败：正本就没通过" >&2; exit 1; }
  # 去掉 V2-12 的测试行
  awk '/^\*\*V2-12\*\*/{f=1} f && /^测试/{f=0; next} {print}' "$good" > "$tmp/no-test.md"
  expect_fail no-test
  # 把 V2-13 改成 V2-12（重复 + 不连续）
  sed 's/^\*\*V2-13\*\*/**V2-12**/' "$good" > "$tmp/dup-id.md"
  expect_fail dup-id
  # 两条规则用同一个测试名
  sed 's/`block_starved_to_max_age_goes_stale`/`block_stale_flip_publishes_twice_same_data`/' "$good" > "$tmp/dup-name.md"
  expect_fail dup-name
  # 测试名不合规
  sed 's/`v2_epoch_changes_on_restart`/`V2 epoch`/' "$good" > "$tmp/bad-name.md"
  expect_fail bad-name
  # 没有规则
  printf '# 空\n' > "$tmp/empty.md"
  expect_fail empty
  echo "self-test ok"
}

case ${1:-} in
  --self-test) self_test ;;
  --impl) check "${2:-$ROOT/docs/STATE_V2.md}" 1 ;;
  *) check "${1:-$ROOT/docs/STATE_V2.md}" 0 ;;
esac
