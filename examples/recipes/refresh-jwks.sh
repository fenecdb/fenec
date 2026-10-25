#!/bin/sh
# Keep the JWKS file fenec-server reads (--jwt-keys) as fresh as the provider's.
# fenec-server fetches no URL itself: it reads this file, and again whenever
# its modification time changes (at most once a second). Run this from cron or
# a systemd timer every few minutes.
#
#   refresh-jwks.sh https://acme.us.auth0.com/.well-known/jwks.json /etc/fenec/jwks.json
#
# The new file is written beside the old one and renamed over it, so the server
# never reads half of one, and only a file holding an RSA key replaces the one
# in use: an error page, an empty body or a provider that went EC-only leaves
# the keys that work where they are, and the job fails for whoever watches it.
set -eu
url=$1
file=$2
tmp=$(mktemp "$file.XXXXXX")
trap 'rm -f "$tmp"' EXIT
curl -fsS --max-time 10 "$url" -o "$tmp"
jq -e '[.keys[]? | select(.kty == "RSA")] | length > 0' "$tmp" >/dev/null ||
    { echo "refresh-jwks: $url holds no RSA key; $file left as it was" >&2; exit 1; }
chmod 644 "$tmp"
mv "$tmp" "$file"
trap - EXIT
