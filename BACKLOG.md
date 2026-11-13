# Backlog

- Web UI design: the UI is unstyled server-rendered HTML (htmx, no CSS). Add one small stylesheet served like htmx.min.js (layout, status badges, tables, forms, dark mode); keep it dependency-free.
- Public URL wiring for apps that need HTTPS (Vaultwarden): tunnel + Access, separate from install.
- Installer: tested 2026-11-13 on fresh Debian 13 + Ubuntu 24.04 VPS (--binary mode); publish a GitHub release (binary + checksum) once the repo is public; musl static build to support older distros.
- Update flow follow-ups: `dockwren update` (CLI) is done and tested locally (bad tag rolls back, good bump updates); still to do: UI "update available" badge + button, restore-from-snapshot (data rollback), test on a fresh VPS, post_start/migration hooks between versions.
