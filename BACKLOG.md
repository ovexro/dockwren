# Backlog

- Web UI design: the UI is unstyled server-rendered HTML (htmx, no CSS). Add one small stylesheet served like htmx.min.js (layout, status badges, tables, forms, dark mode); keep it dependency-free.
- Public URL wiring for apps that need HTTPS (Vaultwarden): tunnel + Access, separate from install.
- Installer: `install.sh` is written but untested on a clean VPS; publish a GitHub release (binary + checksum) once the repo is public; musl static build to support older distros.
- Update flow: bump pinned tag, snapshot first, rollback on failure.
