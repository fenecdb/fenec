#!/bin/sh
# Keep the JWKS file fenec-server reads (--jwt-keys) as fresh as the provider's.
# fenec-server fetches no URL: it reads this file, and again whenever its
# modification time changes (at most once a second). Run this from cron or a
# systemd timer every few minutes.
#
#   refresh-jwks.sh https://acme.us.auth0.com/.well-known/jwks.json /etc/fenec/jwks.json [own-keys.json]
#
# The new file is written beside the old one and renamed over it, so the server
# never reads half of one, and only a file holding a key the server verifies
# with (an RSA one, or an EC one on P-256) replaces the one in use: an error
# page, an empty body or a provider that went EdDSA-only leaves the keys that
# work where they are, and the job fails for whoever watches it.
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
jq -e '[.keys[]? | select(.kty == "RSA" or (.kty == "EC" and .crv == "P-256"))] | length > 0' "$tmp" >/dev/null ||
    { echo "refresh-jwks: $url holds no RSA or P-256 key; $file left as it was" >&2; exit 1; }
if [ -n "$own" ]; then
    # A file of your own keys with none in it is a mistake that would drop them
    # from the server's file, and a kid on both sides is one key claiming
    # another's name: a token that names it is checked against both.
    jq -e '(.keys | type == "array") and (.keys | length > 0)' "$own" >/dev/null ||
        { echo "refresh-jwks: $own holds no key; $file left as it was" >&2; exit 1; }
    jq -s -e '([.[0].keys[].kid, .[1].keys[].kid] | map(select(. != null))) as $k | ($k | length) == ($k | unique | length)' \
        "$tmp" "$own" >/dev/null ||
        { echo "refresh-jwks: a kid is in both $url and $own; $file left as it was" >&2; exit 1; }
    jq -s '{keys: (.[0].keys + .[1].keys)}' "$tmp" "$own" >"$tmp.merged"
    mv "$tmp.merged" "$tmp"
fi
chmod 644 "$tmp"
mv "$tmp" "$file"
trap - EXIT
