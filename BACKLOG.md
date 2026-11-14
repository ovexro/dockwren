# Backlog

Open items only, one line each, with the date it was last verified open. Delivered work lives in `git log`
(delete the line in the SAME commit that delivers it; grep this file for the feature's keywords first).

## Open
- OS updates: not tested on aarch64 Ubuntu (x86_64 Ubuntu 22.04/24.04/26.04, Debian 13 x86_64 and Debian 13 aarch64 on the Pi pass, 2026-11-14). Verified open 2026-11-14.
- OS updates: real-packages run seen on a fresh Ubuntu 24.04 VPS (38 pkgs, exit 0, reboot notice) but not yet on the live server (nothing to install there). Verified open 2026-11-14.
- IDEA (2026-11-14, in order): (1) scheduled backups per app + retention + "last good backup" on the app card; (2) external backup target (S3/B2/SFTP) so backups survive disk loss; (3) more catalog templates (Nextcloud, Immich, Jellyfin, Home Assistant, Grafana, Pi-hole/AdGuard, Syncthing, n8n, Homepage, Ghost, Wiki.js); (4) "update available" badge + optional nightly auto-update using the existing rollback; (5) resource alerts (CPU/RAM/disk thresholds, container down/restart-loop) through the existing channels; (6) panel self-update (`dockwren self-update` / Settings button, SHA256-checked); (7) audit log (logins, install/update/remove); (8) "add stack" from pasted Compose YAML; (9) multi-server over SSH (last).

## Decisions (do not re-propose)
- Name stays "Dockwren" (user, 2026-11-13), despite the pending USPTO "DOCKWARE" filing.
- Repo is public (user approved 2026-11-13); ask before any visibility change.
- Cloudflare Access on `expose` is opt-in (`--access`): Bitwarden/git-HTTPS clients cannot pass an Access login.
- Notifications: ntfy/webhook channels are delivered and confirmed on a phone (2026-11-14); no further channels planned.

- Multi-user / roles: not wanted (user, 2026-11-14). Single admin stays.

## Standing
- At every tag/wrapup with new features re-check README, GitHub description/topics, SECURITY.md, docs/TEMPLATES.md (memory public-repo-freshness).
