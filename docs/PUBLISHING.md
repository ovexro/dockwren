# Publishing apps (HTTPS)

How to give an app a public address: directly with Let's Encrypt, or through a Cloudflare Tunnel.

## Direct HTTPS (Caddy, no Cloudflare)

In an app's "Public URL" dialog, save a hostname, then press Check and Publish directly. Dockwren starts
one Caddy container (`dockwren-caddy`, host networking) that listens on ports 80 and 443, requests a
free Let's Encrypt certificate (HTTP-01) and proxies the hostname to the app. Requirements: an A record
for the hostname pointing at this server's public IP (DNS only, not proxied), and ports 80/443 free on the
server and forwarded to it. The Check button tests these first. A hostname uses either this or the
Cloudflare Tunnel, never both. Certificates are kept in `caddy/data` in the data directory. If `ufw` is active, Check
shows the exact `ufw allow` commands (the panel never changes your firewall). The dialog shows when the
certificate expires; Caddy renews it by itself. The config export keeps which apps are direct, and the
panel restarts Caddy on start, so a restored server re-issues certificates without extra steps.

### DNS challenge variant

"Publish with DNS challenge" proves the domain with a TXT record that Caddy creates in your Cloudflare
account (token with Zone / DNS / Edit, pasted in the Cloudflare tab), instead of answering on port 80.
Use it when port 80 is blocked or behind CGNAT, or when the app should be reachable only on your LAN or
over Tailscale but still have a real certificate (point the hostname's A record at the private address).
The first time, Dockwren builds a Caddy image that includes the Cloudflare DNS plugin; this takes a few
minutes. The token is stored in `caddy/cloudflare.env` (mode 0600) in the data directory.

## Public hostnames (Cloudflare Tunnel)

Each app has a "Public URL" button that records a hostname and shows the tunnel ingress rule to
add. To let Dockwren create the tunnel route and the proxied DNS record, run on the server:

```
CLOUDFLARE_API_TOKEN=... CLOUDFLARE_ACCOUNT_ID=... DOCKWREN_TUNNEL_ID=... \
  dockwren expose <instance> [hostname]          # preview only
  dockwren expose <instance> [hostname] --yes    # apply
  dockwren expose <instance> [hostname] --access you@example.com --yes
  dockwren expose <instance> [hostname] --replace-dns --yes   # delete other DNS records under that name first
                                                 # also put a Cloudflare Access login in front
```

Create the token in the Cloudflare dashboard (My Profile, API Tokens, Create Token, Custom token) with
Account / Cloudflare Tunnel / Edit, Zone / DNS / Edit and Zone / Zone / Read, limited to your account and
the one zone (add Account / Access: Apps and Policies / Edit only for `--access`). The Account ID is on
any zone's Overview page; the Tunnel ID is under Zero Trust, Networks, Tunnels (a remotely managed tunnel).
The same steps are on the panel's Public URL page. It is read from the environment
only. Existing routes and DNS records are never overwritten. Do not put Cloudflare Access in front of
apps used by mobile or desktop clients.

**One-step install and publish (opt-in).** Apps with an address setting (WordPress, Paperless-ngx,
Gitea, Vaultwarden, Miniflux) have an optional "Public hostname" field on the install form (CLI:
`PUBLIC_HOST=blog.example.com`). It sets the app's address to `https://<hostname>` and records the
hostname. If the three variables above are also in the panel's own environment (for example via a
systemd `EnvironmentFile` with mode 0600), the panel publishes it right after the install. By
default they are not set, the panel holds no Cloudflare credentials, and you publish with
`dockwren expose`. Enabling the variables gives anyone who can log in to the panel the power to edit
that tunnel and those DNS records, so use a token scoped to the one zone.

