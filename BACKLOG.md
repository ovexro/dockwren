# Backlog

Open items only, one line each, with the date it was last verified open. Delivered work lives in `git log`
(delete the line in the SAME commit that delivers it; grep this file for the feature's keywords first).

## Open
- OS updates: not tested on aarch64 Ubuntu (x86_64 Ubuntu 22.04/24.04/26.04, Debian 13 x86_64 and Debian 13 aarch64 on the Pi pass, 2026-11-14). Verified open 2026-11-14.
- OS updates: real-packages run seen on a fresh Ubuntu 24.04 VPS (38 pkgs, exit 0, reboot notice) but not yet on the live server (nothing to install there). Verified open 2026-11-14.
- IDEA (2026-11-14): more catalog templates (open-ended; done so far: Nextcloud, Jellyfin, Grafana, n8n, Syncthing, Homepage, Navidrome, Stirling-PDF, Wiki.js, Ghost, SnappyMail, Immich, Roundcube, AdGuard Home, Pi-hole). Items 2-6 of the old list (update badge/auto-update, image heads-up, resource alerts, self-update, audit log, add stack) are delivered.
- IDEA (2026-11-15): more `lan_ports` templates: Home Assistant (host networking, still refused). AdGuard Home and Pi-hole done (Pi-hole has no DHCP: needs cap_add).

- Email: switch dockwren.dev DNS to Purelymail + install SnappyMail live at webmail host (details in memory email-setup). Verified open 2026-11-15.

- Security: check pinned image versions (PHP in Nextcloud, WordPress, SnappyMail) against PHP advisories at each release; PHP2Shell fixed in 8.5.12/8.4.27/8.3.36/8.2.35 (all live PHP apps verified patched 2026-11-15). Verified open 2026-11-15.

## Decisions (do not re-propose)
- Multi-server over SSH: DROPPED by the user (2026-11-15). Stays dropped.
- Name stays "Dockwren" (user, 2026-11-13), despite the pending USPTO "DOCKWARE" filing.
- Repo is public (user approved 2026-11-13); ask before any visibility change.
- Cloudflare Access on `expose` is opt-in (`--access`): Bitwarden/git-HTTPS clients cannot pass an Access login.
- Notifications: ntfy/webhook channels are delivered and confirmed on a phone (2026-11-14); no further channels planned.

- Multi-user / roles: not wanted (user, 2026-11-14). Single admin stays.

## Standing
- At every tag/wrapup with new features re-check README, GitHub description/topics, SECURITY.md, docs/TEMPLATES.md (memory public-repo-freshness).
- IDEA (2026-11-14): off-host follow-ups: one-step disaster recovery (today: `import` the config export, `backup pull`, `restore`; v0.1.30 added export/import, CLI only), "restore from off-host" button in the UI (CLI `backup pull` exists), list remote snapshots, show last upload per app; tested on a real B2 bucket and a full empty-VPS recovery (2026-11-14); not tested on AWS. Not started.
