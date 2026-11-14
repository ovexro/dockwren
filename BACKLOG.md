# Backlog

Open items only, one line each, with the date it was last verified open. Delivered work lives in `git log`
(delete the line in the SAME commit that delivers it; grep this file for the feature's keywords first).

## Open
- OS updates: not tested on aarch64 Ubuntu (x86_64 Ubuntu 22.04/24.04/26.04 and Debian 13 pass). Verified open 2026-11-14.
- OS updates: a run with real packages on the live server not yet seen (live had nothing to install). Verified open 2026-11-14.
- Terminal: the 3-session limit is untested on real connections. Verified open 2026-11-14.
- One-step install + publish: the auto-publish branch is not verified live; needs a scoped Cloudflare token (only the Global API Key exists). Verified open 2026-11-14.
- Backups: `backup.failed` recovery ("Resolved") is only unit-tested on the live status file path. Verified open 2026-11-14.
- Restore: a bare `restore` (skips safety snapshots) is unit-tested but not re-run on a VPS. Verified open 2026-11-14.

## Decisions (do not re-propose)
- Name stays "Dockwren" (user, 2026-11-13), despite the pending USPTO "DOCKWARE" filing.
- Repo is public (user approved 2026-11-13); ask before any visibility change.
- Cloudflare Access on `expose` is opt-in (`--access`): Bitwarden/git-HTTPS clients cannot pass an Access login.
- Notifications: ntfy/webhook channels are delivered and confirmed on a phone (2026-11-14); no further channels planned.

## Standing
- At every tag/wrapup with new features re-check README, GitHub description/topics, SECURITY.md, docs/TEMPLATES.md (memory public-repo-freshness).
