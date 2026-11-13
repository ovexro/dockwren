# Backlog

- Web UI design: the UI is unstyled server-rendered HTML (htmx, no CSS). Add one small stylesheet served like htmx.min.js (layout, status badges, tables, forms, dark mode); keep it dependency-free.
- Backups: run the template's `backup.paths` + `backup.dumps` (pre-update snapshot; restic or a panel job).
- Public URL wiring for apps that need HTTPS (Vaultwarden): tunnel + Access, separate from install.
- Update flow: bump pinned tag, snapshot first, rollback on failure.
