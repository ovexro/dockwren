# Backlog

Open items only, one line each, with the date it was last verified open. Delivered work lives in `git log`
(delete the line in the SAME commit that delivers it; grep this file for the feature's keywords first).

## Open
- More catalog templates (open-ended; ~15 done, see `templates/`). Verified open 2026-11-17.
- Security: PHP advisory recheck after 2026-11-19 (PHP2Shell fixed in 8.5.12/8.4.27/8.3.36/8.2.35; Nextcloud, WordPress, SnappyMail, Roundcube pinned versions patched as of 2026-11-16; 8.5.13/8.4.28 RCs seen).
- Security: WordPress template has 2 fixable criticals needing upstream rebuilds (checked 2026-11-17): apache2 2.4.69 (fix 2.4.71) in wordpress:7.1.4-php8.x-apache; Go stdlib 1.24.6 (fix 1.24.13) in mariadb:11.8.9. Recheck both tags at each release.
- Off-host: not tested on AWS (B2 tested). Low value, keep unless asked.
- Idea: server migration, remaining steps: (2) `recover` re-publishes public hostnames itself when Cloudflare is connected (now only prints `dockwren expose`); (3) optional direct `dockwren migrate` (fresh backup, then transfer, no off-host needed). Step 1 (panel settings in the config export) done 2026-11-17.

## Decisions (do not re-propose)
- Multi-server over SSH: DROPPED by the user (2026-11-15).
- Name stays "Dockwren" (user, 2026-11-13), despite the pending USPTO "DOCKWARE" filing.
- Repo is public (user approved 2026-11-13); ask before any visibility change.
- Cloudflare Access on `expose` is opt-in (`--access`): Bitwarden/git-HTTPS clients cannot pass an Access login.
- Notifications: ntfy/webhook only; no further channels planned (2026-11-14).
- Home Assistant template: DROPPED (2026-11-16): needs host networking, which Dockwren refuses.
- Multi-user / roles: not wanted (2026-11-14). Single admin stays.

## Standing
- BEFORE EVERY TAG run `scripts/docs-check.sh` (README versions/apps/CLI/install options/env vars/links, no private IPs or hostnames, GitHub description/topics/license); fix what it flags. Screenshots: regenerate with `tools/shots.py` (the `apps-*-view.png` frames) into `docs/screenshots/` when the UI changes; never use Overview (shows the real hostname).
- At every tag/wrapup with new features re-check README, GitHub description/topics, SECURITY.md, docs/TEMPLATES.md (memory public-repo-freshness).
