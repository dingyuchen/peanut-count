#!/bin/zsh
# Publishes a recorded flow and prints PR-ready markdown.
# - MP4s and the contact sheet go to the "flow-media" prerelease, which keeps videos out of git history.
# - GIF previews (a few hundred KB each) go to the "flow-media" orphan branch under pr<N>/.
#   Release assets are served as application/octet-stream, so GitHub won't show them as images.
# GitHub only plays videos dragged into its web editor, so the markdown embeds the GIFs, each linking to its MP4.
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
for f in "$dir"/[0-9][0-9]-*.mp4 "$dir/full-flow.mp4" "$dir/contact-sheet.png"; do
  staged=$repo/.flows/.upload/pr$pr-${f:t}
  mkdir -p "${staged:h}"; cp "$f" "$staged"; assets+=("$staged")
done
gh release upload $tag "${assets[@]}" --clobber
rm -rf "$repo/.flows/.upload"

# GIF previews on the orphan branch; link them by commit so a re-record never shows a cached old GIF.
media=$repo/.flows/.media-branch
remote=$(git -C "$repo" remote get-url origin)
rm -rf "$media"
if git ls-remote --exit-code --heads "$remote" flow-media > /dev/null; then
  git clone -q --depth 1 --single-branch --branch flow-media "$remote" "$media"
else
  git init -q "$media" && git -C "$media" checkout -q --orphan flow-media && git -C "$media" remote add origin "$remote"
  printf '# Flow previews\n\nGIF previews for pull requests, written by `.claude/skills/ios-flow-pr/scripts/publish-media.sh`.\n' > "$media/README.md"
fi
rm -rf "$media/pr$pr" && mkdir -p "$media/pr$pr" && cp "$dir"/previews/*.gif "$media/pr$pr/"
git -C "$media" add -A
git -C "$media" commit -q -m "PR #$pr flow previews" || true
git -C "$media" push -q origin flow-media
sha=$(git -C "$media" rev-parse HEAD)
rm -rf "$media"
gifs=https://raw.githubusercontent.com/$slug/$sha/pr$pr

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
  row+="| [<img src=\"$gifs/$name.gif\" width=\"180\">]($base-$name.mp4)<br>\`$name\` $(duration "$f") "
  n=$((n + 1))
  if (( n % 4 == 0 )); then echo "$row|"; row=""; fi
done
if [[ -n $row ]]; then echo "$row|"; fi
