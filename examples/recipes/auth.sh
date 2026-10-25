#!/bin/sh
# docs/auth, run: a fenec-server that takes tokens, held to policy.txt, and
# what it answers a token it should and one it should not. Two servers, one
# after the other:
#
#   1. HS256, the secret a file holds and `--mint-token` signing with it:
#      what to try first, with nothing to sign up for;
#   2. RS256 from a JWKS file, as an identity provider's tokens come: the
#      stand-in provider of idp.mjs publishes the file, and the run rotates
#      its key, breaks the file and sends a token the server does not take.
#
#   FENEC_SERVER=target/debug/fenec-server sh examples/recipes/auth.sh
#
# Every server listens on a port the system picks and says which.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
server=${FENEC_SERVER:-$root/target/debug/fenec-server}
dir=$(mktemp -d)
pid=
idppid=
trap 'stop; [ -z "$idppid" ] || kill "$idppid" 2>/dev/null || true; rm -rf "$dir"' EXIT INT TERM

start() { # start <name> <flags...>: the server over a new file, its address in $url
    name=$1
    shift
    rm -f "$dir/server.log"
    "$server" --file "$dir/$name.fenec" --http 127.0.0.1:0 --http-token root \
        --policy "${policy:-$here/policy.txt}" "$@" 2>"$dir/server.log" &
    pid=$!
    i=0
    until grep -q "listening on: http://" "$dir/server.log" 2>/dev/null; do
        i=$((i + 1))
        [ $i -gt 100 ] && { cat "$dir/server.log"; exit 1; }
        sleep 0.1
    done
    url=$(sed -n 's/.*listening on: \(http:[^ ]*\).*/\1/p' "$dir/server.log" | head -1)
}

stop() {
    [ -z "$pid" ] || kill "$pid" 2>/dev/null || true
    [ -z "$pid" ] || wait "$pid" 2>/dev/null || true
    pid=
}

# call <what> <token> <statement>: one statement to /query, as that token.
call() {
    body=$(node -p 'JSON.stringify({query: process.argv[1]})' "$3")
    printf '%-42s %s\n' "$1" "$(curl -s -X POST "$url/query" -H "Authorization: Bearer $2" \
        -H 'Content-Type: application/json' --data-binary "$body" -w ' [%{http_code}]')"
}

# poll <seconds> <command...>: the command again until it succeeds, for that long at most.
poll() {
    n=$(($1 * 10))
    shift
    while ! "$@" >/dev/null 2>&1; do
        n=$((n - 1))
        [ $n -gt 0 ] || { echo "timed out: $*" >&2; exit 1; }
        sleep 0.1
    done
}
# accepts <token>: whether the server takes the token (a 200 to a read).
accepts() {
    [ "$(curl -s -o /dev/null -w '%{http_code}' -X POST "$url/query" -H "Authorization: Bearer $1" \
        -H 'Content-Type: application/json' -d '{"query": "get notes"}')" = 200 ]
}

# serves <token>: whether /_whoami takes the token.
serves() { [ "$(curl -s -o /dev/null -w '%{http_code}' "$url/_whoami" -H "Authorization: Bearer $1")" = 200 ]; }

setup() { # the collections and two profiles, written with the server's own token
    while read -r stmt; do call "" root "$stmt" >/dev/null; done <"$here/schema.fenecql"
    call "" root 'put profiles [{owner: "alice", name: "Alice", email: "alice@example.test", phone: "111"}, {owner: "bob", name: "Bob", email: "bob@example.test", phone: "222"}]' >/dev/null
}

echo "== 1. HS256: a secret, and --mint-token"
head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' >"$dir/jwt.secret"
mint() { "$server" --jwt-secret-file "$dir/jwt.secret" --mint-token "$1"; }
alice=$(mint '{"sub":"alice"}')
bob=$(mint '{"sub":"bob"}')
admin=$(mint '{"sub":"carol","role":"admin"}')
lapsed=$(mint '{"sub":"alice","exp":1}')
start hs --jwt-secret-file "$dir/jwt.secret"
setup
call "alice writes a note" "$alice" 'put notes {title: "buy oat milk"}'
call "alice writes a note as bob" "$alice" 'put notes {owner: "bob", title: "not mine"}'
call "bob writes a note" "$bob" 'put notes {title: "call the dentist"}'
call "alice reads notes" "$alice" 'get notes select owner, title'
call "bob reads notes" "$bob" 'get notes select owner, title'
call "the admin reads notes" "$admin" 'get notes select owner, title'
call "the admin deletes a note" "$admin" 'del notes where owner = "alice"'
call "everyone reads profiles" "$bob" 'get profiles'
call "bob renames alice" "$bob" 'set profiles {name: "Mallory"} where owner = "alice"'
call "bob changes his email" "$bob" 'set profiles {email: "x@example.test"} where owner = "bob"'
call "bob changes his phone" "$bob" 'set profiles {phone: "333"} where owner = "bob"'
call "alice adds an audit line" "$alice" 'put audit {action: "login", at: now()}'
call "alice deletes it" "$alice" 'del audit where actor = "alice"'
call "a token that has lapsed" "$lapsed" 'get notes'
call "no token" "" 'get notes'
echo "-- /_whoami for alice"
curl -s "$url/_whoami" -H "Authorization: Bearer $alice"
echo
stop

echo
echo "== 2. RS256: the keys of a JWKS file, as an identity provider publishes them"
idp="$dir/idp"
mkdir "$idp"
node "$here/idp.mjs" init "$idp"
node "$here/idp.mjs" serve "$idp" >"$dir/idp.url" &
idppid=$!
poll 10 test -s "$dir/idp.url"
idpurl=$(cat "$dir/idp.url")
JWKS_PROTO='=http' sh "$here/refresh-jwks.sh" "$idpurl/.well-known/jwks.json" "$dir/jwks.json" # what the job does first
start rs --jwt-keys "$dir/jwks.json"
setup
call "" root 'put notes {owner: "bob", title: "bob note"}' >/dev/null
for pair in clerk:user_2xYzAbc auth0:0123456789 supabase:7c9e6679-7425-40de-944b-e07fc1f90ae7 \
            cognito:a1b2c3d4-5678-90ab-cdef-111111111111 access:3d9a0f0e-5c1b-4a3e-8f61-2b7d9c4e1a55; do
    shape=${pair%%:*}
    token=$(node "$here/idp.mjs" token "$idp" "$shape" "${pair#*:}")
    sub=$(curl -s "$url/_whoami" -H "Authorization: Bearer $token" | jq -r .sub)
    call "" root "put notes {owner: \"$sub\", title: \"$shape note\"}" >/dev/null
    call "$shape: sub $sub" "$token" 'get notes select owner, title'
done
token=$(node "$here/idp.mjs" token "$idp" plain user_2xYzAbc)
echo "-- the provider rotates: a new key signs, and the server's file has not heard"
node "$here/idp.mjs" add-key "$idp" idp-2
token=$(node "$here/idp.mjs" token "$idp" plain user_2xYzAbc)
call "a token of the new key" "$token" 'get notes select title'
echo "-- the refresh job fetches the provider's file; no restart"
JWKS_PROTO='=http' sh "$here/refresh-jwks.sh" "$idpurl/.well-known/jwks.json" "$dir/jwks.json"
poll 5 accepts "$token"
call "the same token" "$token" 'get notes select title'
echo "-- a key of your own service goes into the file beside the provider's"
node "$here/idp.mjs" service-key "$idp"
JWKS_PROTO='=http' sh "$here/refresh-jwks.sh" "$idpurl/.well-known/jwks.json" "$dir/jwks.json" "$idp/service-jwks.json"
jq -c '[.keys[].kid]' "$dir/jwks.json"
service=$(node "$here/idp.mjs" service-token "$idp" files-backend)
poll 5 serves "$service"
printf '%-42s ' "the server's view of the service's token"
curl -s "$url/_whoami" -H "Authorization: Bearer $service" | jq -c '{kind, sub}'
echo "-- the job asks for a page that is not there one night"
JWKS_PROTO='=http' sh "$here/refresh-jwks.sh" "$idpurl/nothing" "$dir/jwks.json" 2>&1 || echo "(the job failed, rc $?)"
call "the same token" "$token" 'get notes select title'
echo "-- and a file that is no JWKS, written over the keys by hand"
echo '<html>502 Bad Gateway</html>' >"$dir/jwks.json"
# The server reads the file again when a request asks for the keys, so a request is what makes it say so.
noted() { accepts "$token" || true; grep -q 'the keys read before stay' "$dir/server.log"; }
poll 5 noted
call "the same token" "$token" 'get notes select title'
grep -o 'jwks.json: .*' "$dir/server.log" | sed 's/ request_id=.*//' | sed "s#$dir/##"
echo "-- a provider that signs with EC keys"
call "an ES384 token" "$(node "$here/idp.mjs" ec-token user_2xYzAbc)" 'get notes select title'
stop

echo
echo "== 3. rules that name a claim's value, every one pinned to this application"
policy="$here/policy-claims.txt"
cp "$idp/jwks.json" "$dir/jwks.json"
start claims --jwt-keys "$dir/jwks.json"
setup
call "" root 'put staff {user: "auth0|dana"}' >/dev/null
call "" root 'put notes [{owner: "auth0|ada", title: "auth0 note"}, {owner: "ada", title: "ada note"}, {owner: "bob", title: "bob note"}]' >/dev/null
t() { node "$here/idp.mjs" token "$idp" "$@"; }
API='https://api.example.test'
call "auth0, a token for the API" "$(t auth0 ada '{"permissions":[]}')" 'get notes select title'
call "auth0, one holding read:notes" "$(t auth0 ada)" 'get notes select title'
call "auth0, an ID token (aud is the client)" "$(t auth0 ada '{"aud":"Zr8Example"}')" 'get notes select title'
call "auth0, a token for another API" "$(t auth0 ada '{"aud":["https://other.example.test"]}')" 'get notes select title'
call "no aud, holding read:notes" "$(t plain ada '{"permissions":["read:notes"]}')" 'get notes select title'
call "profiles, a token for the API" "$(t auth0 ada)" 'get profiles'
call "profiles, a token for another API" "$(t auth0 ada '{"aud":["https://other.example.test"]}')" 'get profiles'
call "dana, in staff, a token for the API" "$(t auth0 dana '{"permissions":[]}')" 'get notes select title'
call "dana, in staff, another API's" "$(t auth0 dana '{"aud":["https://other.example.test"],"permissions":[]}')" 'get notes select title'
call "access, this application" "$(t access ada)" 'get notes select title'
call "access, another application of the team" "$(t access ada '{"aud":["0123456789abcdef"]}')" 'get notes select title'
call "firebase, this project" "$(t firebase ada)" 'get notes select title'
call "firebase, another project" "$(t firebase ada '{"aud":"other-project"}')" 'get notes select title'
call "clerk, this origin" "$(t clerk ada '{"azp":"https://app.example.test"}')" 'get notes select title'
call "clerk, another origin" "$(t clerk ada '{"azp":"https://evil.example.test"}')" 'get notes select title'
call "cognito, an access token of this client" "$(t cognito ada)" 'get notes select title'
call "cognito, an ID token" "$(t cognito ada '{"token_use":"id","aud":"1example23456789"}')" 'get notes select title'
call "supabase, a person" "$(t supabase ada)" 'get notes select title'
call "supabase, an anonymous sign-in" "$(t supabase ada '{"is_anonymous":true}')" 'get notes select title'
stop
unset policy

echo
echo "== 4. a JWKS file with no key this server can use is refused at the start"
# A P-384 key, for ES384: not one of the algorithms this server verifies.
printf '{"keys":[{"kty":"EC","crv":"P-384","kid":"ec-1","alg":"ES384","use":"sig","x":"Q873GS5VgaO8sPd7q3e0CLqJr3pjeijRJVNwydrSUPgBcf5Ykn_H60fVfPKBOt9D","y":"lcvLR6f1kd9OIf53hL0pGOwn5ayn655WIT9HNTuheW4jgCo8v7Xj461tr8QVSJwl"}]}' >"$dir/ec.json"
"$server" --http 127.0.0.1:0 --jwt-keys "$dir/ec.json" --policy "$here/policy.txt" 2>&1 | sed "s#$dir/##" || true

echo
echo "== 5. turning a person away before their token expires"
policy="$here/policy-ban.txt"
cp "$idp/jwks.json" "$dir/jwks.json"
start ban --jwt-keys "$dir/jwks.json"
setup
call "" root 'put notes [{owner: "alice", title: "alice note"}, {owner: "bob", title: "bob note"}]' >/dev/null
bobs=$(node "$here/idp.mjs" token "$idp" plain bob)
call "bob reads" "$bobs" 'get notes select title'
call "" root 'put banned {user: "bob"}' >/dev/null
call "bob reads, banned, the same token" "$bobs" 'get notes select title'
call "bob writes, banned" "$bobs" 'put notes {title: "still here"}'
call "alice reads" "$(node "$here/idp.mjs" token "$idp" plain alice)" 'get notes select title'
stop
unset policy

echo
echo "== 6. a tenant node: the tenant claim"
tenant_node() { # tenant_node <shape> <tenant> <other tenant> [fenec-server flags...]
    shape=$1
    mine=$2
    other=$3
    shift 3
    rm -f "$dir/jwks.json" "$dir/server.log"
    rm -rf "$dir/tenants"
    mkdir "$dir/tenants"
    cp "$idp/jwks.json" "$dir/jwks.json"
    "$server" --dir "$dir/tenants" --http 127.0.0.1:0 --http-token root --admin-token admin \
        --jwt-keys "$dir/jwks.json" --policy "$here/policy.txt" "$@" 2>"$dir/server.log" &
    pid=$!
    poll 10 grep -q "listening on: http://" "$dir/server.log"
    url=$(sed -n 's/.*listening on: \(http:[^ ]*\).*/\1/p' "$dir/server.log" | head -1)
    owner=alice
    [ "$shape" != auth0 ] || owner='auth0|alice' # Auth0's sub is the identity provider's, then the user's
    for t in $mine $other; do
        curl -s -X PUT "$url/_admin/tenants/$t" -H 'Authorization: Bearer admin' >/dev/null
        curl -s -X POST "$url/t/$t/query" -H 'Authorization: Bearer root' -H 'Content-Type: application/json' \
            -d '{"query": "create collection notes (owner text @hash, title text, body text)"}' >/dev/null
        curl -s -X POST "$url/t/$t/query" -H 'Authorization: Bearer root' -H 'Content-Type: application/json' \
            -d "{\"query\": \"put notes {owner: \\\"$owner\\\", title: \\\"$t note\\\"}\"}" >/dev/null
    done
    tcall() { # tcall <what> <token> <tenant>
        printf '%-46s %s\n' "$1" "$(curl -s -X POST "$url/t/$3/query" -H "Authorization: Bearer $2" \
            -H 'Content-Type: application/json' -d '{"query": "get notes select title"}' -w ' [%{http_code}]')"
    }
    token=$(node "$here/idp.mjs" token "$idp" "$shape" alice)
    tcall "alice, token for $mine, asks $mine" "$token" "$mine"
    tcall "alice, token for $mine, asks $other" "$token" "$other"
    tcall "a token naming no tenant, asks $mine" "$(node "$here/idp.mjs" token "$idp" plain alice)" "$mine"
    stop
}
echo "-- the claim \`tenant\`, which Clerk's session token carries once customized"
tenant_node clerk acme globex
echo "-- --jwt-tenant-claim org_id, which Auth0 puts in a token for an organization"
tenant_node auth0 org_acme org_globex --jwt-tenant-claim org_id

echo
echo "== 7. a legacy HS256 secret, as an oct key of the file"
secret=$(head -c 32 /dev/urandom | base64 | tr '+/' '-_' | tr -d '=\n')
printf '{"keys":[{"kty":"oct","kid":"legacy","alg":"HS256","k":"%s"}]}' "$secret" >"$dir/oct.json"
sed 's/"k":"[^"]*"/"k":"..."/' "$dir/oct.json"
echo
legacy=$("$server" --jwt-keys "$dir/oct.json" --policy "$here/policy.txt" --mint-token '{"sub":"alice"}')
start legacy --jwt-keys "$dir/oct.json"
setup
call "" root 'put notes {owner: "alice", title: "alice note"}' >/dev/null
call "a token signed with the secret" "$legacy" 'get notes select owner, title'
stop
