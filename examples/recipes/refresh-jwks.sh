#!/bin/sh
# Keep the JWKS file fenec-server reads (--jwt-keys) as fresh as the provider's.
# fenec-server fetches no URL: it reads this file, and again whenever its
# modification time changes (at most once a second). Run this from cron or a
# systemd timer every few minutes.
#
#   refresh-jwks.sh https://acme.us.auth0.com/.well-known/jwks.json /etc/fenec/jwks.json [own-keys.json]
#
# The new file is written beside the old one and renamed over it, so the server
# never reads half of one, and only a file holding an RSA key replaces the one
# in use: an error page, an empty body or a provider that went EC-only leaves
# the keys that work where they are, and the job fails for whoever watches it.
#
# A third argument is a JWKS of keys of your own -- the public halves of the
# keys your backend or Worker signs its short service tokens with -- which go
# into the file beside the provider's. Without it the file would hold only the
# provider's keys, and a service could not call the server.
#
# Only https, and no more than 1 MiB: a URL that redirects to http, or one that
# answers with a body of any size, is not a keys file. JWKS_PROTO='=http' lifts
# the first, for the stand-in provider of idp.mjs on localhost.
set -eu
url=$1
file=$2
own=${3:-}
tmp=$(mktemp "$file.XXXXXX")
trap 'rm -f "$tmp" "$tmp.merged"' EXIT
curl -fsS --proto "${JWKS_PROTO:-=https}" --proto-redir "${JWKS_PROTO:-=https}" --max-redirs 3 \
    --max-filesize 1048576 --max-time 10 "$url" -o "$tmp"
jq -e '[.keys[]? | select(.kty == "RSA")] | length > 0' "$tmp" >/dev/null ||
    { echo "refresh-jwks: $url holds no RSA key; $file left as it was" >&2; exit 1; }
if [ -n "$own" ]; then
    jq -s '{keys: (.[0].keys + .[1].keys)}' "$tmp" "$own" >"$tmp.merged"
    mv "$tmp.merged" "$tmp"
fi
chmod 644 "$tmp"
mv "$tmp" "$file"
trap - EXIT
