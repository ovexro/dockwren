# Backlog

Open items only, one line each, with the date it was last verified open. Delivered work lives in `git log`
(delete the line in the SAME commit that delivers it; grep this file for the feature's keywords first).

## Open
- PLAN (user approved all five, 2026-11-16; build in this order, one release each when ready):
  STATUS: (1) released v0.1.32, (2) restore test + off-host fetch button released v0.1.33. (3) disk usage per app released v0.1.34. (4) update window + skip version released v0.1.35. (5) third-party template sources released v0.1.36 (design below, delivered; installs now record their source; not done: no live reload without restart). Plan complete.
  (1) One-command recovery: `dockwren recover` = import + `backup pull` + `restore` for every app; also push the encrypted config export to the off-host target after each backup (never stale, survives disk loss); handle offsite.toml/credentials without hand-copying (prompt or re-enter); fold in the readable-bundle hint and the "restore from off-host" UI button / list remote snapshots.
  (5) Extra template sources. DESIGN (proposed 2026-11-16, awaiting user OK):
      - Source: `dockwren template add <git-url> <full-commit-sha>` (also a Settings form); https only, no branch/tag tracking. Fetch once (shallow, no submodules, no hooks, size and file-count caps), keep a copy under <data>/templates/<id>/, record url+sha in a sources file.
      - Checks: same `template::validate` as bundled (no host ports outside `lan_ports`, no privileged/host network/pid, no host mounts outside the app folder); also refuse any id that equals a bundled id (the existing extra_dir mechanism silently OVERRIDES bundled templates: close that for pinned sources).
      - Approval: show manifest + compose (images, ports, mounts, post_start commands) and require an explicit "Add" click; a new sha for the same source is a new approval with a diff, like app updates.
      - Marking: catalog shows a "third-party" label with url + short sha; installs record the source.
      - Not doing: auto-updating sources, running anything from the repo at fetch time, private repos/credentials (v1).
      - Open question: post_start commands run inside containers; allow for third-party, or refuse in v1 (recommended: refuse).
- PLAN 2 (user approved all five, 2026-11-16; build in this order, one release each, verify the item is still open before starting):
  (1) released v0.1.38 (src/logcap.rs; Cap logs button + auto on install/update): Docker log size cap: default max-size/max-file in every app's compose (override file) + a button for already installed apps.
  (2) released v0.1.39 (src/webcheck.rs; probe every 5 min, 3 fails raise app.unreachable, status in Apps): external check of public URLs.
  (3) released v0.1.40 (src/clone.rs; Duplicate button on the Restore page, no CLI command): clone app from a snapshot under a new name and port.
  (4) released v0.1.41 (src/vulnscan.rs; Trivy 0.76.2 pinned, daily, counts on Apps, notify on fixable criticals; CLI `dockwren scan`): image vulnerability scan.
  (5) released v0.1.42 (Passwords button: show again after re-auth; rotate only for secrets marked `rotate = true`, Pi-hole): generated passwords.
- IDEA (2026-11-14): more catalog templates (open-ended; done so far: Nextcloud, Jellyfin, Grafana, n8n, Syncthing, Homepage, Navidrome, Stirling-PDF, Wiki.js, Ghost, SnappyMail, Immich, Roundcube, AdGuard Home, Pi-hole). Items 2-6 of the old list (update badge/auto-update, image heads-up, resource alerts, self-update, audit log, add stack) are delivered.

- Security: check pinned image versions (PHP in Nextcloud, WordPress, SnappyMail) against PHP advisories at each release; PHP2Shell fixed in 8.5.12/8.4.27/8.3.36/8.2.35 (all PHP apps incl. Roundcube verified patched 2026-11-16 at v0.1.37, no newer security release found; 8.5.13/8.4.28 RCs seen, recheck after 2026-11-19). Verified open 2026-11-16.

## Decisions (do not re-propose)
- Multi-server over SSH: DROPPED by the user (2026-11-15). Stays dropped.
- Name stays "Dockwren" (user, 2026-11-13), despite the pending USPTO "DOCKWARE" filing.
- Repo is public (user approved 2026-11-13); ask before any visibility change.
- Cloudflare Access on `expose` is opt-in (`--access`): Bitwarden/git-HTTPS clients cannot pass an Access login.
- Notifications: ntfy/webhook channels are delivered and confirmed on a phone (2026-11-14); no further channels planned.

- Home Assistant template: DROPPED by the user (2026-11-16): needs host networking, which Dockwren refuses by design. Stays dropped.
- Multi-user / roles: not wanted (user, 2026-11-14). Single admin stays.

## Standing
- At every tag/wrapup with new features re-check README, GitHub description/topics, SECURITY.md, docs/TEMPLATES.md (memory public-repo-freshness).
- IDEA (2026-11-14): off-host follow-ups: one-step disaster recovery (today: `import` the config export, `backup pull`, `restore`; v0.1.30 added export/import, CLI only), "restore from off-host" button in the UI (CLI `backup pull` exists), list remote snapshots, show last upload per app; tested on a real B2 bucket and a full empty-VPS recovery (2026-11-14); not tested on AWS. Not started.
