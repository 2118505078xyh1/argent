#!/usr/bin/env bash
# 发行版阈值检查（规则见 CLAUDE.md「Release policy」）。
#
# 上次发行版 tag 以来累计改动的 Rust 行数（added+deleted，git numstat，
# 只算已提交内容）÷ 当前 Rust 总行数 > 5% 时输出 release=needed 并以
# 退出码 0 结束；否则 release=not-needed、退出码 1。
# 阈值可用 ARGENT_RELEASE_THRESHOLD 覆盖，默认 5%。
set -euo pipefail
cd "$(dirname "$0")/.."

threshold="${ARGENT_RELEASE_THRESHOLD:-5}"
tag=$(git describe --tags --abbrev=0 2>/dev/null || true)
if [ -z "$tag" ]; then
  echo "error: no release tag found" >&2
  exit 1
fi

total=$(git ls-files '*.rs' | xargs wc -l | tail -1 | awk '{print $1}')
changed=$(git diff "$tag" HEAD --numstat -- '*.rs' | awk '{a+=$1; d+=$2} END {print a+d}')
ratio=$(awk "BEGIN{printf \"%.2f\", $changed * 100 / $total}")
needed=$(awk "BEGIN{print ($changed * 100 > $total * $threshold) ? \"needed\" : \"not-needed\"}")

echo "tag=$tag total=$total changed=$changed ratio=${ratio}% threshold=${threshold}%"
echo "release=$needed"
[ "$needed" = "needed" ]
