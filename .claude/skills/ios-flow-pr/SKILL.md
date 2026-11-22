---
name: ios-flow-pr
description: Verify a user-facing change to the Peanut Count iOS app by recording the onboarding and home flows as simulator video clips, checking them, and opening or updating the PR with the clips attached. Use before opening or updating any PR that touches apps/ios (src, app.json, modules, native config, copy or styling), or when asked to verify, record, or demo the app flow.
---

# iOS flow verification and PR

Every PR that changes something a user can see or touch in `apps/ios` includes the
flow clips recorded from that PR's code, plus a verification table. Backend-only,
docs-only, or test-only PRs skip the clips. Their PR body says
"No user-facing change", with the reason.

Pieces:

- `harness/Tests/Flow.swift` is the XCUITest that drives the app.
- `scripts/record.sh` builds, installs, seeds the fixtures, records the clips and the joined `full-flow.mp4`, then makes GIF previews and a contact sheet.
- `scripts/publish-media.sh <pr>` uploads the media to the `flow-media` prerelease and prints the PR markdown.
- `flows.md` lists the clips and the standing checks.

Output goes to `.flows/<branch>/`, which is gitignored.

## Steps

1. **Unit tests**: `cd apps/ios && npm test`. Fix any failures first.

2. **Update the flow if the UI changed shape.** A new or renamed screen, button, label or step needs a matching step in `Flow.swift` (find elements by accessibility label) and a row in `flows.md`. Keep the clip numbering continuous.

3. **Record.** Run `.claude/skills/ios-flow-pr/scripts/record.sh` in the background; it takes about 6 to 10 minutes.
   - It runs only on the dedicated simulator named `PeanutCount Flows` (set `FLOW_SIM` to use another), creating it if needed, and erases it on every full run.
   - Never boot, erase, install on or record any other simulator. The developer's own booted simulators are off limits.
   - `SKIP_BUILD=1` reuses the last Release build.
   - If a run fails partway, read `.flows/.harness/<test>.log`, then resume with `record.sh "" testResumeChart` (or `testResumeAmbiguous` or `testResumeDark`). Clips recorded earlier in the run are kept.
   - Right after an erase, the first document-picker sessions list files but don't open them. `record.sh` runs `testWarmUp` first, which keeps relaunching until the picker has opened each fixture once, imports nothing, and records nothing. Keep that step if you edit the script.

4. **Verify.**
   - Open `contact-sheet.png` and the stills behind each check in `flows.md`, plus checks specific to this change (what the PR claims to fix or add).
   - Measure anything pixel-sensitive from the 3× PNGs instead of judging by eye.
   - Write a table with one row per check: expectation | PASS/FAIL | still(s).
   - Fix each FAIL and re-record before opening the PR. If a FAIL is deliberately left, say why in the PR.

5. **Open or update the PR.**
   - Commit and push the branch, then open the PR against `main`. Use `pull_request_create` when available, otherwise `gh pr create`.
   - Run `.claude/skills/ios-flow-pr/scripts/publish-media.sh <pr-number>`. This creates the `flow-media` prerelease if needed and replaces this PR's earlier assets.
   - Put its output in the PR body, under the summary. Use `gh api -X PATCH repos/<owner>/<repo>/pulls/<n> -F body=@<file>`; `gh pr edit` fails on this repo because of the Projects (classic) deprecation.
   - When later pushes change user-facing behavior, re-record and re-run the publish step; the assets are overwritten in place.

## PR body template

```markdown
## Summary
- …

## Verification
`npm test`: N passed. Flow recorded from <short sha> on iPhone 17 / iOS <ver>.

| Check | Result | Evidence |
| --- | --- | --- |
| … | PASS | `stills/12-home-november.png` |

<output of publish-media.sh>
```

## Notes

- Why release assets: GitHub plays inline video only when it's uploaded through the web editor. Committing MP4s would bloat git history. Prerelease assets are free to replace. The GIF previews render inline and each one links to the full-resolution MP4.
- The capture server listens on `127.0.0.1:18745`. If a stale server holds the port, find it with `lsof -i :18745` and stop it.
- For an exploratory click-through of a brand-new flow (before it's scripted), a delegate agent can drive the dedicated simulator. Fold what it learned back into `Flow.swift` so the next PR is scripted.
