# Backlog

Open items only, one line each, with the date it was last verified open. Delivered work lives in `git log`
(delete the line in the SAME commit that delivers it; grep this file for the feature's keywords first).

## Open
- More catalog templates (open-ended; ~15 done, see `templates/`). Verified open 2026-11-17.
- Security: PHP advisory recheck (PHP2Shell fixed in 8.5.12/8.4.27/8.3.36/8.2.35; all pinned apps patched, rechecked 2026-11-19 (again 17:00, wordpress:7.1.5-php8.3-apache rebuilt 16:06 but still php 8.3.36): php 8.5.13 etc. exist on Docker Hub but app images (wordpress 7.1.5 = php 8.3.36, nextcloud 35.0.3 = 8.5.12, roundcube 1.7.6 = 8.4.27) not rebuilt yet; recheck at next release).
- Security: WordPress template has 2 fixable criticals needing upstream rebuilds (rechecked 2026-11-19 17:00 by running the image: wordpress 7.1.5 out, still apache 2.4.69 (fix 2.4.71); mariadb 11.8.10 not yet, Go stdlib 1.24.6 in mariadb:11.8.9, fix 1.24.13). Recheck both tags at each release.
- Off-host: not tested on AWS (B2 tested). Low value, keep unless asked.
- Ideas queue (proposed and verified not built/not excluded 2026-11-17; user approved all, order = recommendation):
  1. Adopt running containers / existing Compose projects as managed stacks: phases 1 (scan) + 2 (adopt: named volumes + host folders backed up in place, v0.1.80; restore test/clone skipped for host folders) built; open: ports (backups of named volumes built 2026-11-18, real-Docker test passes; verified with a real adopted Postgres project 2026-11-18: backup, verify, restore, restore-test) claimed in .env, ProtectHome files.
  Non-code: list Dockwren in awesome-selfhosted + public announcement (outward action: ask the user first).
- Ideas queue 2 (proposed and verified not built/not excluded 2026-11-18; user approved; order = recommendation):
  (queue 3, 2026-11-19: stack version history v0.1.71 done; needs between apps v0.1.72 done, to be tried live; public status page open, low value.; registry logins v0.1.73 done (verified against a local authenticated registry); scheduled stop/start per app v0.1.74 done (real-Docker test passes, not yet tried live); drift detection (v0.1.95): "out of sync" line + Apply button per app, hash compare via `compose config --hash`.)
  (queue 2 done: PWA v0.1.67-68; Edit stack for pasted stacks v0.1.69, for adopted stacks v0.1.70; remove confirmation + Delete app v0.1.70.)
- Passkeys (released v0.1.59, tested on a real device 2026-11-17): reauth prompts (Passwords, terminal, updates) still take a typed code (TOTP/recovery), not a passkey tap; passkeys work only on the hostname they were added on.

## Decisions (do not re-propose)
- Multi-server over SSH: DROPPED by the user (2026-11-15).
- Name stays "Dockwren" (user, 2026-11-13), despite the pending USPTO "DOCKWARE" filing.
- Repo is public (user approved 2026-11-13); ask before any visibility change.
- Cloudflare Access on `expose` is opt-in (`--access`): Bitwarden/git-HTTPS clients cannot pass an Access login.
- Notifications: ntfy/webhook; Telegram ADDED as a third channel by user decision 2026-11-19 (separate section under ntfy, not a dropdown; fake-server test, real bot test by user later; also a 640x640 Telegram profile logo from static/icon-512.png). No other channels planned.
- Home Assistant template: DROPPED (2026-11-16): needs host networking, which Dockwren refuses.
- Multi-user / roles: not wanted (2026-11-14). Single admin stays.
- Server migration: DONE via config export (panel settings included) + `dockwren recover` (v0.1.56, tested on a real VPS 2026-11-17). Direct `dockwren migrate` without an off-host target: DROPPED (2026-11-17), off-host covers the real case. The tunnel connector (cloudflared) is not managed by Dockwren: the user runs it on the new server.

## Standing
- BEFORE EVERY TAG run `scripts/docs-check.sh` (README versions/apps/CLI/install options/env vars/links, no private IPs or hostnames, GitHub description/topics/license); fix what it flags. Screenshots: regenerate with `tools/shots.py` (the `apps-*-view.png` frames) into `docs/screenshots/` when the UI changes; never use Overview (shows the real hostname).
- NEW FEATURE = add its keyword to `scripts/features.txt` in the same release: docs-check then fails until README AND site/index.html mention it (added 2026-11-19 after the site lagged ~12 features).
- At every tag/wrapup with new features re-check README, GitHub description/topics, SECURITY.md, docs/TEMPLATES.md (memory public-repo-freshness).
- Screenshots: reshot 2026-11-19 at 1920x1080 (c615bfc). Site redeployed. OPEN: optional Resources reshot after 30+ min history.
- Site images soft at display size: reshoot at 1440x810, device_scale_factor=2 (shots_real.py), redeploy; one-column gallery already deployed (2026-11-19).
