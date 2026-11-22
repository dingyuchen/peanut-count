#!/bin/zsh
# Uploads a recorded flow to the repo's "flow-media" prerelease and prints PR-ready markdown.
# Release assets keep the videos out of git history. GitHub only plays videos dragged
# into its web editor, so the markdown embeds GIF previews and links each MP4.
# Usage: publish-media.sh <pr-number> [clip-dir]
set -euo pipefail
skill=${0:A:h:h}
repo=$(git -C "$skill" rev-parse --show-toplevel)
pr=$1
branch=$(git -C "$repo" branch --show-current)
dir=${2:-$repo/.flows/${branch//\//-}}
tag=flow-media
slug=$(gh repo view --json nameWithOwner -q .nameWithOwner)
gh release view $tag > /dev/null 2>&1 || gh release create $tag --prerelease --target main --title "PR flow recordings" \
  --notes "Simulator recordings attached to pull requests by .claude/skills/ios-flow-pr. Assets are named pr<number>-<clip>." > /dev/null

assets=()
for f in "$dir"/[0-9][0-9]-*.mp4 "$dir/full-flow.mp4" "$dir"/previews/*.gif "$dir/contact-sheet.png"; do
  staged=$repo/.flows/.upload/pr$pr-${f:t}
  mkdir -p "${staged:h}"; cp "$f" "$staged"; assets+=("$staged")
done
gh release upload $tag "${assets[@]}" --clobber
rm -rf "$repo/.flows/.upload"

base=https://github.com/$slug/releases/download/$tag/pr$pr
duration() { awk -F'\t' -v f="${1:t}" '$1 == f { s = $2; printf "%d:%02d", s / 60, s % 60 }' "$dir/durations.tsv"; }
echo "## Flow recordings"
echo
echo "Recorded on an iPhone 17 simulator from a fresh install (\`.claude/skills/ios-flow-pr\`). GIFs play at 2x; click a clip for the full-resolution MP4."
echo
echo "**[▶ Full flow ($(duration "$dir/full-flow.mp4"))]($base-full-flow.mp4)** · [contact sheet]($base-contact-sheet.png)"
echo
echo "| | | | |"
echo "|:-:|:-:|:-:|:-:|"
row=""; n=0
for f in "$dir"/[0-9][0-9]-*.mp4; do
  name=${f:t:r}
  row+="| [<img src=\"$base-$name.gif\" width=\"180\">]($base-$name.mp4)<br>\`$name\` $(duration "$f") "
  n=$((n + 1))
  if (( n % 4 == 0 )); then echo "$row|"; row=""; fi
done
[[ -n $row ]] && echo "$row|"
