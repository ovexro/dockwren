#!/usr/bin/env bash
# Sign a draft release with the offline ed25519 key, then publish it.
# Usage: DOCKWREN_SIGN_KEY=/path/to/release.key scripts/sign-release.sh vX.Y.Z
# The key (PEM, ed25519) must NOT live on a build machine or in the repo. Steps: download the draft's
# files, check every binary against SHA256SUMS, sign SHA256SUMS, verify the signature with the public
# key embedded in the code, upload SHA256SUMS.sig, publish.
set -euo pipefail
cd "$(dirname "$0")/.."
TAG="${1:?usage: sign-release.sh vX.Y.Z}"
KEY="${DOCKWREN_SIGN_KEY:?set DOCKWREN_SIGN_KEY to the private key file}"
[ -r "$KEY" ] || { echo "cannot read $KEY" >&2; exit 1; }
REPO=ovexro/dockwren
W="$(mktemp -d)"
trap 'rm -rf "${W:?}"' EXIT
gh release download "$TAG" --repo "$REPO" --dir "$W" --pattern 'dockwren-*-linux' --pattern SHA256SUMS
(cd "$W" && sha256sum -c SHA256SUMS)
[ "$(wc -l < "$W/SHA256SUMS")" -eq 2 ] || { echo "SHA256SUMS must list exactly the two binaries" >&2; exit 1; }
openssl pkeyutl -sign -inkey "$KEY" -rawin -in "$W/SHA256SUMS" -out "$W/SHA256SUMS.sig"
# Verify with the public key the installer ships, so a wrong private key is caught before publishing.
PUB="$(sed -n '/<<.PUBKEY.$/,/^PUBKEY$/p' install.sh | sed '1d;$d')"
printf '%s\n' "$PUB" > "$W/pub.pem"
openssl pkeyutl -verify -pubin -inkey "$W/pub.pem" -rawin -in "$W/SHA256SUMS" -sigfile "$W/SHA256SUMS.sig" >/dev/null
gh release upload "$TAG" --repo "$REPO" "$W/SHA256SUMS.sig"
gh release edit "$TAG" --repo "$REPO" --draft=false
echo "signed and published $TAG"
