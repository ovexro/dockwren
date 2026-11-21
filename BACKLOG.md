# Backlog

Open items only, one line each, with the date it was last verified open. Delivered work lives in `git log`
(delete the line in the SAME commit that delivers it; grep this file for the feature's keywords first).

## Open
- Hardening (2026-11-21, ideas from code scan): (a) security headers DONE 2026-11-21 (phase 1 + 2: CSP with `script-src 'self'`, no inline handlers/scripts, test `no_inline_scripts_or_handlers`, htmx `allowEval:false`, UI test `~/work/tools/test_csp_ui.py`); (b) DONE 2026-11-21: `audit.yml` (cargo audit on Cargo.lock changes + weekly); watch: RUSTSEC-2026-0374 basic-toml unmaintained via askama (build-time only), drop the `--ignore` when askama moves off it; (c) DONE 2026-11-21: `ci.yml` on push/PR (fmt, clippy, test, gitleaks, docs-check; first run green); (d) split big files (`apps.rs` 4600 lines, `main.rs` 3000, `backup.rs` 1500); (e) proptest/fuzz on user-input parsers (template, configbundle, config import); (f) DONE 2026-11-21: restore-test is scheduled weekly (Settings → Backups, **Test restores**); DONE 2026-11-21: on by default for a fresh setup, old saved schedules unchanged, hint shown when backups are on without it; Sessions panel in Settings → Security (list + Sign out everywhere else, test `~/work/tools/test_sessions_ui.py`); `lan_port_free` now asks the kernel by bind first (the /proc/net tables can omit live sockets under load: measured ~14% of reads), test made deterministic. (2026-11-21)
- More catalog templates (34 in `apps/`, all VPS-tested; Kavita tested 2026-11-20: install, library scan, backup, restore; image has curl+tar, no wget). Next: Paperless-ai; check `tar` in the image first. Forgejo reserves the name admin (default user forgejo-admin). (2026-11-20)
- Security: PHP recheck at next release: wordpress 7.1.5 = PHP 8.3.37 OK; nextcloud 35.0.3 = 8.5.12, roundcube 1.7.6 = 8.4.27 (8.5.13 exists, images not rebuilt). (2026-11-20)
- Security: mariadb 11.8.9 ships gosu with Go 1.24.6 (fix 1.24.13); 11.8.10 not out. Recheck at each release. (2026-11-20)
- Off-host: not tested on AWS (B2 tested). Low value. (2026-11-20)
- UPS (NUT, released v0.1.98), user postponed, do not push: clean-shutdown test (`upsmon -c fsd`), `battery.charge.low` 25% (needs upsd admin user), optional panel button via root helper, `dockwren setup-ups` untested on fresh machine/Ubuntu/older NUT. journalctl showed no nut-monitor/dockwren entries during the test: check unit names. (2026-11-20)
- Adopt containers/Compose projects (phases 1+2 built, v0.1.80): open: ports claimed in .env, ProtectHome files, restore test/clone for host folders. (2026-11-18)
- Public status page: low value. (2026-11-19)
- Non-code: list Dockwren in awesome-selfhosted + public announcement (outward: ask the user first).
- Passkeys: reauth prompts (Passwords, terminal, updates) still take a typed code, not a passkey tap; passkeys work only on the hostname they were added on. (2026-11-17)

## Decisions (do not re-propose)
- Vikunja template: DROPPED (2026-11-20): its image has no `tar`/shell, so Dockwren cannot back it up (checked on a real VPS). Homebox replaced it.
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
- Screenshots: reshot 2026-11-19 at 1920x1080 (c615bfc). Site redeployed.
- Screenshots reshot 2026-11-20 at 1440x810 @2x (2880x1620), Resources with 30+ min history. Site redeploy: see commit log.
