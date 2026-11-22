# Backlog

Open items only, one line each, with the date it was last verified open. Delivered work lives in `git log`
(delete the line in the SAME commit that delivers it; grep this file for the feature's keywords first).

## Open
- CSP: web terminal page alone keeps `style-src 'unsafe-inline'` (xterm.js injects <style> at runtime); revisit if xterm gets a nonce/CSSOM-only mode. (2026-11-21)
- Hardening left: external independent review; Docker socket proxy (later); optional re-gate of stored third-party templates at load (model_gate async, load_all sync). Roadmap: memory hardening-roadmap. (2026-11-21)
- Watch: `audit.yml` ignores RUSTSEC-2026-0374 (basic-toml via askama, build-time); drop `--ignore` when askama moves off it. Big files if wanted: `install.rs`, `template.rs`. (2026-11-21)
- More catalog templates: 40 in `apps/`, all VPS-tested (Planka added 2026-11-22); none queued, pick a popular app. Test notes per app live in memory `pitfalls-vps-templates`. (2026-11-22)
- Security recheck at each release: PHP (nextcloud 35.0.3 = 8.5.12, roundcube 1.7.6 = 8.4.27; 8.5.13 exists, images not rebuilt); mariadb 11.8.9 gosu Go 1.24.6 (fix 1.24.13; 11.8.10 not out). (2026-11-20)
- UPS (NUT, v0.1.98), user postponed, do not push: `upsmon -c fsd` shutdown test, `battery.charge.low` 25% (needs upsd admin user), optional panel button, `dockwren setup-ups` untested on fresh/Ubuntu/older NUT; check nut unit names in journalctl. (2026-11-20)
- Adopt containers/Compose (phases 1+2 built, v0.1.80): open: ports claimed in .env, ProtectHome files, restore test/clone for host folders. (2026-11-18)
- Non-code: list Dockwren in awesome-selfhosted + public announcement (outward: ask the user first).
- Passkeys: reauth prompts (Passwords, terminal, updates) still take a typed code; passkeys work only on the hostname they were added on. (2026-11-17)

## Decisions (do not re-propose)
- Freeze prevention DONE (v0.1.113 fix, v0.1.115 watchdog, `scripts/smoke.sh` after every deploy). Clippy `significant_drop_*` lints DROPPED (21 style hits, they miss the original double-lock bug). Existing installs get the watchdog drop-in only via `sudo dockwren self-update` or re-running install.sh.
- File Browser template DROPPED (2026-11-21, upstream archived, no security fixes). Dozzle DROPPED (needs docker.sock). Excalidraw skipped (no version tags; revisit). Forgejo reserves the name admin (default user forgejo-admin).
- Vikunja template: DROPPED (2026-11-20): its image has no `tar`/shell, so Dockwren cannot back it up (checked on a real VPS). Homebox replaced it.
- Multi-server over SSH: DROPPED by the user (2026-11-15).
- Name stays "Dockwren" (user, 2026-11-13), despite the pending USPTO "DOCKWARE" filing.
- Repo is public (user approved 2026-11-13); ask before any visibility change.
- Cloudflare Access on `expose` is opt-in (`--access`): Bitwarden/git-HTTPS clients cannot pass an Access login.
- Notifications: ntfy/webhook; Telegram ADDED as a third channel by user decision 2026-11-19 (separate section under ntfy, not a dropdown; fake-server test, real bot test by user later; also a 640x640 Telegram profile logo from static/icon-512.png). No other channels planned.
- Home Assistant template: DROPPED (2026-11-16): needs host networking, which Dockwren refuses.
- Multi-user / roles: not wanted (2026-11-14). Single admin stays.
- Server migration: DONE via config export (panel settings included) + `dockwren recover` (v0.1.56, tested on a real VPS 2026-11-17). Direct `dockwren migrate` without an off-host target: DROPPED (2026-11-17), off-host covers the real case. The tunnel connector (cloudflared) is not managed by Dockwren: the user runs it on the new server.
- Off-host on AWS and a public status page: DROPPED (2026-11-21), low value (B2 is tested).

## Standing
- BEFORE EVERY TAG run `scripts/docs-check.sh` (README versions/apps/CLI/install options/env vars/links, no private IPs or hostnames, GitHub description/topics/license); fix what it flags. Screenshots: regenerate with `tools/shots.py` (the `apps-*-view.png` frames) into `docs/screenshots/` when the UI changes; never use Overview (shows the real hostname).
- NEW FEATURE = add its keyword to `scripts/features.txt` in the same release: docs-check then fails until README AND site/index.html mention it (added 2026-11-19 after the site lagged ~12 features).
- At every tag/wrapup with new features re-check README, GitHub description/topics, SECURITY.md, docs/TEMPLATES.md (memory public-repo-freshness).
- Screenshots last reshot 2026-11-20 at 1440x810 @2x (Resources with 30+ min history); site redeployed.
- Seafile template (user wish, 2026-11-22): MariaDB+Redis+seafile, test on OVH; user wants file data on the HDD (/mnt/backup, spin-down 10 min is fine), DB/Redis on SSD; desktop client on SER8. DECISION (user asked, I recommended): Tailscale-only, hostname seafile.dockwren.dev = DNS-only A record to the server Tailscale IP (Cloudflare write = confirm + infra-write marker; check how Dockwren gets the HTTPS cert, DNS challenge?); check SER8 is on Tailscale. Still ask: folder path; offsite/restic copy of the HDD data. Memory reference-backup has the spin-down facts.
