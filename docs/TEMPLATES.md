# Writing a Dockwren template

An app in Dockwren is a directory with two files: a `manifest.toml` that tells the panel what to ask,
check and back up, and a plain Docker Compose file. Bundled templates live in `apps/`. To try your own
without rebuilding, point `DOCKWREN_APPS` at a directory of templates (one sub-directory each); a
template there replaces a bundled one with the same `id`.

```
apps/<id>/
  manifest.toml
  compose.yaml
  icon.svg          # optional
```

`icon.svg` is shown in the catalog and the installed-apps list. Use a square `viewBox` (the bundled
ones are 64x64 with rounded corners) and plain shapes: it is served as an image with a locked-down
Content-Security-Policy, so scripts and external references do nothing. Bundled templates must have one.

Compose does the variable substitution. Dockwren writes the values to the instance's `.env` (mode 0600)
and runs `docker compose --env-file .env`, so `compose.yaml` is copied verbatim and stays runnable by hand.

## A minimal template

`manifest.toml`:

```toml
[app]
id = "hello"                      # [a-z0-9-]+
name = "Hello"
description = "Tiny static web server"
version = 1                        # bump when compose, images or migrations change
needs_public_https = false         # true = unusable remotely without HTTPS (shows a note)
public_url_field = "PUBLIC_URL"    # optional: field holding the app's address; the install form's
                                   # "Public hostname" fills it with https://<host> and hides it
accepts_public_host = false        # optional: app has no address setting, but still gets the
                                   # "Public hostname" field (it only publishes, sets no variable)

[[fields]]
key = "INSTANCE_NAME"
label = "Name"
type = "string"
default = "hello"
pattern = "^[a-z][a-z0-9-]{1,30}$"

[[fields]]
key = "WEB_PORT"
label = "Web port"
type = "port"
default = "auto"                   # the panel picks a free port

[[services]]
name = "hello"
main = true                        # the service the "open" link and status refer to
health = { source = "panel", test = ["CMD-SHELL", "nc -z 127.0.0.1 80"], interval = "15s", start_period = "10s" }

[backup]
paths = ["data"]                   # relative to the instance directory
```

`compose.yaml`:

```yaml
services:
  hello:
    image: nginx:1.27-alpine
    restart: unless-stopped
    volumes:
      - ./data:/usr/share/nginx/html   # created for you, and backed up (see [backup])
    ports:
      - "127.0.0.1:${WEB_PORT}:80"
```

## Manifest reference

**`[app]`**: `id`, `name`, `version` are required. Bump `version` only when the compose file, an image tag or a migration changes: a bump makes `dockwren update` back up and recreate every installed copy, so do not bump for UI-only edits (labels, help, notes). Optional: `description`, `homepage`, `needs_public_https`.

**`[[fields]]`** are the install form, in order. Keys are `UPPER_SNAKE_CASE`.
- `type`: `string`, `password`, `int`, `bool`, `port`, `url`, `email`, `select`.
- Optional: `default`, `pattern` (string/password only), `required` (default true), `help`,
  `options` (required for `select`), `min`, `max`.
- `INSTANCE_NAME` is reserved and always present; define the field to give it a label and default.
- A `default` may reference earlier fields as `${KEY}`. `port` accepts `auto`.
- Everything is validated again on the server.

**`[[secrets]]`** are generated at install and never shown in the form.
- `kind`: `password`, `hex`, or `argon2` (stores a hash in `.env`).
- `length` (minimum 16, default 32). `reveal = true` shows the plaintext once after install (for
  an admin password); it is not stored anywhere except the instance's `.env`.

**`[[services]]`** list the Compose services you care about, by name.
- Exactly one has `main = true`.
- `health = { source = "image" }` uses the image's own healthcheck.
- `health = { source = "panel", test = [...], interval, start_period }` injects a healthcheck through a
  generated `compose.override.yaml`, so your `compose.yaml` stays untouched.
- Install waits until every service is healthy.

**`[[post_start]]`** run once after the first install, in order, via `docker compose exec`:
`name`, `service`, `run` (argv list), optional `user`, optional
`capture = { label, regex }` (one capture group over stdout; the value is shown once, not stored).

**`[[migrations]]`** run after an update that crosses `to_version`
(instance version < `to_version` <= template version), oldest first. Same fields as `post_start`
without `capture`, plus `to_version` (2 or higher). A failing migration rolls the update back
(the old compose files return; data is not rolled back, the snapshot path is printed).

**`[backup]`**
- `paths`: directories under the instance dir, archived as root. May be empty if you only use dumps.
- `[[backup.dumps]]`: `service`, `run` (writes the dump to stdout), `restore` (reads it on stdin), and
  `output` (plain file name). Use dumps for databases; never archive a live database directory.

**`[update]`**
- `image_pins`: images an update may move to a newer tag.
- `pre_update = ["backup"]`: snapshot before every update.
- `pinned_major`: images whose major version an update must not change (databases need a manual migration).

**Variables.** `${KEY}` works in compose files, `default`, `run` and `test`, and resolves against
fields then secrets. An unknown variable is a load error.

## What Dockwren refuses to load

The compose file is linted line by line, then checked with `docker compose config`:
- any published port that does not bind `127.0.0.1` (expose apps publicly with `dockwren expose`);
- `privileged`, `network_mode`, `pid: host`, `cap_add`, or a Docker socket mount;
- images without a pinned tag, or with `:latest`;
- unknown `${VARIABLES}`, duplicate keys or services, secrets shorter than 16 characters;
- backup paths that are absolute, empty, or contain `..`.

## Test it

```
DOCKWREN_APPS=./my-templates dockwren install hello      # install and wait until healthy
dockwren backup hello && dockwren restore hello         # round-trip the data
dockwren update hello                                    # no-op until you bump version
dockwren remove hello --yes
```

Check three things before sharing a template: the app's web UI answers, data written after a backup is
gone after a restore, and a bad image tag rolls an update back instead of leaving it broken.

## Writing field help

Write `help` for someone who has never used Docker. Say what the value is for, what the user sees or types, and whether it can be changed later. Avoid jargon (origin, IANA, ingress); give a concrete example. Say what a field is **not** when it is easy to confuse (for example the internal name is not part of the website address).
