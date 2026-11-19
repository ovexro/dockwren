# Adding your own stack

Catalog > **Add a stack from a Compose file**: paste a `docker-compose.yml`, give it a name, optionally
paste the variables it uses (`KEY=value` lines, what you would put in `.env`), and Dockwren checks and
starts it. From then on it is a normal app in the list: start/stop, logs, alerts, backups of `./data`,
a public hostname for the first published port.

## How the file is checked

The file is not trusted. Dockwren asks `docker compose config` for the exact model Docker would run
(anchors, short syntax and `${VARIABLES}` resolved with your variables) and checks that against an
allow-list. Whatever is not on the list is refused with the reason, so pasting YAML cannot hand over the host.

Allowed: `image`, `command`, `entrypoint`, `environment`, `ports`, `expose`, `volumes`, `tmpfs`,
`depends_on`, `restart`, `healthcheck`, `networks`, `labels`, `user`, `working_dir`, `hostname`,
`container_name`, `init`, `read_only`, `cap_drop`, `dns`, `extra_hosts`, `logging`, `ulimits`,
memory/CPU limits, `deploy.resources`, `profiles`, `x-` extension keys. Top level: `services`, `volumes`, `networks`.

Rules that trip people up:

- **Ports must bind `127.0.0.1`**: write `"127.0.0.1:8080:80"`, not `"8080:80"`. Reach the app from
  outside through a public hostname (Cloudflare Tunnel), not an open port.
- **Bind mounts only under `./data`** (for example `./data/db:/var/lib/postgresql/data`). Named volumes
  are fine. `/`, `/etc`, the Docker socket and the stack's own `.env` and `compose.yaml` are refused.
- **Every service needs an `image`**: `build:` is refused. For a private image, first add a login under Settings → System → Registry logins.
- **Not supported**: `privileged`, `cap_add`, `devices`, `security_opt`, `sysctls`, `network_mode`, `pid`,
  `ipc`, `volumes_from`, `include`, `extends`, `env_file`, `secrets`, `configs`, external volumes or
  networks, volume `driver_opts`, non-bridge networks. Software that needs these (Pi-hole/AdGuard DNS on
  port 53, Home Assistant with host networking) cannot be added this way.
- The words `include:`, `extends:`, `env_file`, `build:`, `secrets:` and `configs:` are refused anywhere in
  the text, even in a comment, because they make Docker read other files on the server.

## What you get and what you do not

- **Backups** cover `./data` of the stack. Data in named volumes is not included: use `./data/...` bind mounts for what you want backed up.
- **Updates**: Dockwren does not update a pasted stack (it has no idea which tag is newer). Edit the
  image tag in the stack's `compose.yaml` and run `docker compose up -d` from its folder, or remove and re-add it.
- The stack lives in `<data>/instances/<name>/`, like catalog apps, and shows as app `custom`.

## Editing a stack later

Apps → **Edit stack** (pasted and adopted stacks) shows the saved file and environment. Saving runs the same checks as adding a stack, takes a backup of `./data`, then pulls the images and recreates the containers. If a service exits or is unhealthy, the old file and environment are put back and started again. A published port may change to another free port; For pasted stacks the Dockwren-owned variables (`INSTANCE_NAME`, `WEB_PORT`) are not editable. An adopted stack keeps its own `.env` exactly as written, its original Compose project, and the adoption rule (named volumes only, no host bind mounts).

Every successful edit keeps the file it replaced (the last 20, under the stack's folder). **Earlier versions** at the bottom of the form lists them: **Load** fills the form with that version and shows the changes from the current file; nothing runs until you press the button, so the usual checks, backup and rollback apply to a restore too.

## Adopting a project that is already running

Instead of pasting a file you can adopt a Compose project that already runs on the server (Apps → Add an app → Scan for other Compose projects). The same checks apply, plus: one compose file, named volumes only (no host bind mounts), and the copy must produce exactly the same `docker compose config --hash` as the running project, so adopting never restarts or recreates containers. Backups of an adopted app also cover its named volumes: the stack is stopped for a few seconds while each volume is archived (so databases stay consistent), then started again; restore, verify, restore tests and off-host copies work the same as for other apps. Removing an adopted app deletes its volumes. The panel cannot read compose files under `/home`.
