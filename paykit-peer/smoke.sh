#!/usr/bin/env bash
# Two scripted peers, A and B, with fresh identities, pay each other on the local regtest
# through the local Dispatch and the fake Homeserver:
#
#   1. both enrol, join and allow each other;
#   2. A requests 10,000 sats from B; B pays on-chain; A verifies the proof and confirms; B
#      receives the receipt;
#   3. the reverse: B requests from A;
#   4. A publishes a Private Payment List to B and requests with `--any`; B pays the list's
#      address.
#
# Every step is asserted from the peers' JSON event lines, and every txid is checked in
# bitcoind. Local test only: every address it talks to is loopback.
#
# Needs: the local Dispatch (client endpoint 127.0.0.1:18580), the fake Homeserver
# (127.0.0.1:18590), bitcoind regtest (127.0.0.1:18443, admin1/123) with a wallet `default`
# that can fund, jq and curl.
#
# Environment:
#   CARGO_TARGET_DIR  where the peer is built (default: <repo>/target)
#   PEER_BIN          a built paykit-peer to use instead of building one
#   SMOKE_DIR         where state and logs go (default: $CARGO_TARGET_DIR/paykit-peer-smoke/<time>)
#   SMOKE_TIMEOUT_S   how long each wait lasts (default 90)
#   BITCOIND_URL, RPC_USER, RPC_PASSWORD  bitcoind's RPC (defaults above)
#   KEEP_SMOKE_DIR=1  keep the state and logs after a pass (always kept after a failure)
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/.." && pwd)
target=${CARGO_TARGET_DIR:-$repo/target}
timeout_s=${SMOKE_TIMEOUT_S:-90}
bitcoind_url=${BITCOIND_URL:-http://127.0.0.1:18443}
rpc_user=${RPC_USER:-admin1}
rpc_password=${RPC_PASSWORD:-123}
sats=10000

say() { printf '== %s\n' "$*" >&2; }
fail() {
    printf 'FAIL: %s\n' "$*" >&2
    printf 'state and logs kept in %s\n' "$work" >&2
    exit 1
}

for tool in jq curl; do
    command -v "$tool" >/dev/null || { echo "smoke: $tool is needed" >&2; exit 1; }
done

if [ -z "${PEER_BIN:-}" ]; then
    say "building paykit-peer"
    (cd "$repo" && cargo build --locked -p paykit-peer >&2)
    PEER_BIN=$target/debug/paykit-peer
fi
[ -x "$PEER_BIN" ] || { echo "smoke: $PEER_BIN is not executable" >&2; exit 1; }

work=${SMOKE_DIR:-$target/paykit-peer-smoke/$(date +%Y%m%d-%H%M%S)-$$}
mkdir -p "$work"
say "state and logs in $work"

# bitcoind JSON-RPC: rpc [wallet] method params-json
rpc() {
    local wallet=$1 method=$2 params=$3 url=$bitcoind_url
    [ -n "$wallet" ] && url=$bitcoind_url/wallet/$wallet
    curl -sS --max-time 30 -u "$rpc_user:$rpc_password" -H 'content-type: application/json' \
        --data "{\"jsonrpc\":\"1.0\",\"id\":\"smoke\",\"method\":\"$method\",\"params\":$params}" "$url"
}

say "preflight"
chain=$(rpc "" getblockchaininfo '[]' | jq -r '.result.chain') || true
[ "$chain" = regtest ] || { echo "smoke: bitcoind at $bitcoind_url is not regtest (got '$chain')" >&2; exit 1; }
curl -sf --max-time 5 http://127.0.0.1:18590/__health >/dev/null \
    || { echo "smoke: the fake Homeserver on 127.0.0.1:18590 is not answering" >&2; exit 1; }
curl -s --max-time 5 -o /dev/null http://127.0.0.1:18580/ \
    || { echo "smoke: the Dispatch on 127.0.0.1:18580 is not answering" >&2; exit 1; }
# Load the funding wallet (not loaded after a node restart).
rpc "" loadwallet '["default"]' >/dev/null || true

pids=()
cleanup() {
    exec 3>&- 4>&- 2>/dev/null || true
    for pid in "${pids[@]:-}"; do
        [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
    done
}
trap cleanup EXIT

peer() { "$PEER_BIN" --state "$work/$1" "${@:2}"; }

say "init A and B (fresh identities), each funded with 500,000 sats"
peer a init --fund 500000 >"$work/a.init" 2>"$work/a.init.err" || fail "A init: $(cat "$work/a.init.err")"
peer b init --fund 500000 >"$work/b.init" 2>"$work/b.init.err" || fail "B init: $(cat "$work/b.init.err")"
a_id=$(peer a id)
b_id=$(peer b id)
a_wallet=$(jq -r 'select(.event=="ready").wallet' "$work/a.init")
b_wallet=$(jq -r 'select(.event=="ready").wallet' "$work/b.init")
say "A = $a_id ($a_wallet)"
say "B = $b_id ($b_wallet)"
[ "$a_id" != "$b_id" ] || fail "A and B have the same identity"

# Start both with `run`, stdin from a FIFO held open on fd 3 (A) and 4 (B).
mkfifo "$work/a.in" "$work/b.in"
peer a run --auto-pay --auto-confirm --poll-ms 500 <"$work/a.in" >"$work/a.out" 2>"$work/a.err" &
pids+=($!)
exec 3>"$work/a.in"
peer b run --auto-pay --auto-confirm --poll-ms 500 <"$work/b.in" >"$work/b.out" 2>"$work/b.err" &
pids+=($!)
exec 4>"$work/b.in"

to_a() { printf '%s\n' "$*" >&3; }
to_b() { printf '%s\n' "$*" >&4; }

# wait_for <peer a|b> <jq filter selecting the event> <what>: print the first matching
# event (compact JSON), or fail after the timeout.
wait_for() {
    local who=$1 filter=$2 what=$3 deadline line
    deadline=$(($(date +%s) + timeout_s))
    while :; do
        line=$(jq -c "select($filter)" "$work/$who.out" 2>/dev/null | head -n 1)
        if [ -n "$line" ]; then
            printf '%s\n' "$line"
            return 0
        fi
        if jq -e 'select(.event=="fatal")' "$work/$who.out" >/dev/null 2>&1; then
            fail "$who stopped: $(jq -c 'select(.event=="fatal")' "$work/$who.out")"
        fi
        [ "$(date +%s)" -lt "$deadline" ] || fail "$who: no $what within ${timeout_s}s"
        sleep 0.3
    done
}

# check_tx <txid> <address> <sats> <wallet of the payee>: the transaction is in bitcoind and
# pays at least <sats> to <address>.
check_tx() {
    local txid=$1 address=$2 want=$3 wallet=$4 got
    got=$(rpc "$wallet" gettransaction "[\"$txid\", true, true]" \
        | jq -r --arg a "$address" '[.result.decoded.vout[] | select(.scriptPubKey.address==$a) | .value * 100000000 | round] | add // 0')
    [ "${got:-0}" -ge "$want" ] || fail "tx $txid pays $got sats to $address, wanted $want"
    say "bitcoind: $txid pays $got sats to $address"
}

wait_for a '.event=="running"' "running" >/dev/null
wait_for b '.event=="running"' "running" >/dev/null

say "admission: A allows B, B allows A"
to_a "allow $b_id"
to_b "allow $a_id"
wait_for a ".event==\"allowed\" and .id==\"$b_id\"" "allowed B" >/dev/null
wait_for b ".event==\"allowed\" and .id==\"$a_id\"" "allowed A" >/dev/null

# pay_flow <payee a|b> <payer a|b> <payer id> <payee id> <payee wallet> [--any <list address>]
# The payee requests $sats from the payer; the payer auto-pays; the payee verifies and
# confirms; the payer gets the receipt. Prints the txid.
pay_flow() {
    local payee=$1 payer=$2 payer_id=$3 payee_id=$4 payee_wallet=$5 any=${6:-} list_address=${7:-}
    local sent hash address paid txid paid_address verified confirmed receipt n
    n=$(jq -c 'select(.event=="request_sent")' "$work/$payee.out" | wc -l | tr -d ' ')
    if [ -n "$any" ]; then
        "to_$payee" "request $payer_id $sats --any"
    else
        "to_$payee" "request $payer_id $sats"
    fi
    # The request this call sent: the (n+1)th request_sent.
    local deadline=$(($(date +%s) + timeout_s))
    while :; do
        sent=$(jq -c 'select(.event=="request_sent")' "$work/$payee.out" | sed -n "$((n + 1))p")
        [ -n "$sent" ] && break
        if jq -e 'select(.event=="command_failed")' "$work/$payee.out" >/dev/null 2>&1; then
            fail "$payee: $(jq -c 'select(.event=="command_failed")' "$work/$payee.out" | tail -n 1)"
        fi
        [ "$(date +%s)" -lt "$deadline" ] || fail "$payee: no request_sent"
        sleep 0.3
    done
    hash=$(jq -r '.request_hash' <<<"$sent")
    [ "$hash" != null ] || fail "$payee: request_sent without a request hash: $sent"
    [ "$(jq -r '.to' <<<"$sent")" = "$payer_id" ] || fail "request_sent to the wrong payer: $sent"
    [ "$(jq -r '.sats' <<<"$sent")" = "$sats" ] || fail "request_sent for the wrong amount: $sent"
    if [ -n "$any" ]; then
        [ "$(jq -r '.destination' <<<"$sent")" = any ] || fail "not an --any request: $sent"
        address=$list_address
    else
        address=$(jq -r '.address' <<<"$sent")
    fi
    say "$payee -> $payer: request $hash for $sats sats to ${address}"

    wait_for "$payer" ".event==\"request_received\" and .request_hash==\"$hash\" and .from==\"$payee_id\" and .sats==$sats" \
        "request_received $hash" >/dev/null
    say "$payer received the request"
    paid=$(wait_for "$payer" ".event==\"paid\" and .request_hash==\"$hash\"" "paid $hash")
    txid=$(jq -r '.txid' <<<"$paid")
    paid_address=$(jq -r '.address' <<<"$paid")
    [ "$(jq -r '.payee' <<<"$paid")" = "$payee_id" ] || fail "paid the wrong payee: $paid"
    [ "$(jq -r '.endpoint_id' <<<"$paid")" = btc-regtest-p2wpkh ] || fail "paid another endpoint: $paid"
    [ "$paid_address" = "$address" ] || fail "paid $paid_address, not $address: $paid"
    [ "$(jq -r '.sats' <<<"$paid")" -ge "$sats" ] || fail "paid less than asked: $paid"
    [ "$(jq -r '.proof_msg_id' <<<"$paid")" != null ] || fail "paid without sending a proof: $paid"
    [[ $txid =~ ^[0-9a-f]{64}$ ]] || fail "paid without a txid: $paid"
    say "$payer paid on-chain: $txid"
    check_tx "$txid" "$address" "$sats" "$payee_wallet"

    wait_for "$payee" ".event==\"proof_received\" and .request_hash==\"$hash\" and .payer==\"$payer_id\"" \
        "proof_received $hash" >/dev/null
    verified=$(wait_for "$payee" ".event==\"verified\" and .request_hash==\"$hash\"" "verified $hash")
    [ "$(jq -r '.txid' <<<"$verified")" = "$txid" ] || fail "verified another tx: $verified"
    [ "$(jq -r '.sats_received' <<<"$verified")" -ge "$sats" ] || fail "verified less: $verified"
    say "$payee verified the proof on-chain"
    confirmed=$(wait_for "$payee" ".event==\"confirmed\" and .request_hash==\"$hash\"" "confirmed $hash")
    receipt=$(jq -r '.receipt_msg_id' <<<"$confirmed")
    [ "$receipt" != null ] || fail "confirmed without a receipt: $confirmed"
    say "$payee confirmed; receipt $receipt"
    wait_for "$payer" ".event==\"receipt_received\" and .request_hash==\"$hash\" and .msg_id==\"$receipt\" and .txid==\"$txid\" and .payee==\"$payee_id\"" \
        "receipt_received $receipt" >/dev/null
    say "$payer received the receipt"
    printf '%s\n' "$txid"
}

say "scene: A requests from B"
tx1=$(pay_flow a b "$b_id" "$a_id" "$a_wallet")
say "scene: B requests from A"
tx2=$(pay_flow b a "$a_id" "$b_id" "$b_wallet")

say "scene: A publishes a Private Payment List to B, then requests with --any"
to_a "publish-list $b_id"
published=$(wait_for a ".event==\"list_published\" and .to==\"$b_id\"" "list_published")
list_address=$(jq -r '.address' <<<"$published")
[[ $list_address == bcrt1q* ]] || fail "the list's address is not P2WPKH: $published"
say "A's list for B: $list_address"
tx3=$(pay_flow a b "$b_id" "$a_id" "$a_wallet" --any "$list_address")

say "mining a block; each payment must confirm"
mine_to=$(rpc default getnewaddress '[]' | jq -r '.result')
rpc "" generatetoaddress "[1, \"$mine_to\"]" >/dev/null
for pair in "$tx1:$a_wallet" "$tx2:$b_wallet" "$tx3:$a_wallet"; do
    txid=${pair%%:*}
    wallet=${pair#*:}
    confs=$(rpc "$wallet" gettransaction "[\"$txid\"]" | jq -r '.result.confirmations')
    [ "${confs:-0}" -ge 1 ] || fail "$txid has $confs confirmations"
    say "$txid confirmed ($confs)"
done

say "stopping the peers"
to_a quit
to_b quit
exec 3>&- 4>&-
for pid in "${pids[@]}"; do wait "$pid" || fail "a peer exited non-zero"; done
pids=()
for who in a b; do
    rpc "" unloadwallet "[\"$(jq -r 'select(.event=="ready").wallet' "$work/$who.init")\"]" >/dev/null || true
done
grep -h '"event":"warning"' "$work/a.out" "$work/b.out" >&2 || true

echo "SMOKE PASSED: A=$a_id B=$b_id txids $tx1 $tx2 $tx3"
if [ -z "${KEEP_SMOKE_DIR:-}" ]; then
    rm -rf "$work"
else
    echo "state and logs kept in $work"
fi
