# Backlog

Open items only, one line each, with the date it was last verified open. Delivered work lives in `git log`
(delete the line in the SAME commit that delivers it; grep this file for the feature's keywords first).

## Open
- More catalog templates (open-ended; ~15 done, see `templates/`). Verified open 2026-11-17.
- Security: PHP advisory recheck after 2026-11-19 (PHP2Shell fixed in 8.5.12/8.4.27/8.3.36/8.2.35; Nextcloud, WordPress, SnappyMail, Roundcube pinned versions patched as of 2026-11-16; Rechecked 2026-11-17: 8.5.13/8.4.28/8.3.37/8.2.36 not yet on Docker Hub; pinned images still on patched versions).
- Security: WordPress template has 2 fixable criticals needing upstream rebuilds (rechecked 2026-11-17, images unchanged since 2026-11-11, no wordpress 7.1.5 / mariadb 11.8.10 yet): apache2 2.4.69 (fix 2.4.71) in wordpress:7.1.4-php8.x-apache; Go stdlib 1.24.6 (fix 1.24.13) in mariadb:11.8.9. Recheck both tags at each release.
- Off-host: not tested on AWS (B2 tested). Low value, keep unless asked.
- Ideas queue (proposed and verified not built/not excluded 2026-11-17; user approved all, order = recommendation):
  1. Adopt running containers / existing Compose projects as managed stacks: phases 1 (scan) + 2 (adopt, named volumes only) built; open: host bind mounts (data relocation), ports (backups of named volumes built 2026-11-18, real-Docker test passes; verified with a real adopted Postgres project 2026-11-18: backup, verify, restore, restore-test) claimed in .env, ProtectHome files.
  Non-code: list Dockwren in awesome-selfhosted + public announcement (outward action: ask the user first).
- Ideas queue 2 (proposed and verified not built/not excluded 2026-11-18; user approved; order = recommendation):
  1. Disk-full forecast: from the 30-day disk history show "full in ~N days" on Overview and alert under 14 days.
  2. Log search/filter in the log viewer (text, time range, level) plus download; today only the last 200 lines.
  3. SMART disk health in Health check and notifications (read-only `smartctl`).
  4. Edit pasted/adopted stacks after install (same strict validation, snapshot before applying).
  5. Installable panel (PWA): web manifest + icon.
- Passkeys (released v0.1.59, tested on a real device 2026-11-17): reauth prompts (Passwords, terminal, updates) still take a typed code (TOTP/recovery), not a passkey tap; passkeys work only on the hostname they were added on.

## Decisions (do not re-propose)
- Multi-server over SSH: DROPPED by the user (2026-11-15).
- Name stays "Dockwren" (user, 2026-11-13), despite the pending USPTO "DOCKWARE" filing.
- Repo is public (user approved 2026-11-13); ask before any visibility change.
- Cloudflare Access on `expose` is opt-in (`--access`): Bitwarden/git-HTTPS clients cannot pass an Access login.
- Notifications: ntfy/webhook only; no further channels planned (2026-11-14).
- Home Assistant template: DROPPED (2026-11-16): needs host networking, which Dockwren refuses.
- Multi-user / roles: not wanted (2026-11-14). Single admin stays.
- Server migration: DONE via config export (panel settings included) + `dockwren recover` (v0.1.56, tested on a real VPS 2026-11-17). Direct `dockwren migrate` without an off-host target: DROPPED (2026-11-17), off-host covers the real case. The tunnel connector (cloudflared) is not managed by Dockwren: the user runs it on the new server.

## Standing
- BEFORE EVERY TAG run `scripts/docs-check.sh` (README versions/apps/CLI/install options/env vars/links, no private IPs or hostnames, GitHub description/topics/license); fix what it flags. Screenshots: regenerate with `tools/shots.py` (the `apps-*-view.png` frames) into `docs/screenshots/` when the UI changes; never use Overview (shows the real hostname).
- At every tag/wrapup with new features re-check README, GitHub description/topics, SECURITY.md, docs/TEMPLATES.md (memory public-repo-freshness).
