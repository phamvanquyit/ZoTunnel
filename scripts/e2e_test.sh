#!/bin/bash
set -e

# E2E: local HTTP → zotunnel → zo-tunnel-server → curl

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

SERVER_BIN="$PROJECT_DIR/target/release/zo-tunnel-server"
CLIENT_BIN="$PROJECT_DIR/target/release/zotunnel"

CONTROL_PORT=16200
PUBLIC_PORT=16210
LOCAL_PORT=13000
LOCAL_PORT2=13001
TOKEN="test_secret_42"
DOMAIN="test.localhost"
CONFIG_DIR=$(mktemp -d)
export HOME="$CONFIG_DIR"

cleanup() {
    echo "Cleaning up..."
    kill $HTTP_PID $HTTP2_PID $SERVER_PID $CLIENT_PID $CLIENT2_PID 2>/dev/null || true
    wait $HTTP_PID $HTTP2_PID $SERVER_PID $CLIENT_PID $CLIENT2_PID 2>/dev/null || true
    rm -rf "$CONFIG_DIR"
}
trap cleanup EXIT

echo "Zo Tunnel E2E Test"
echo ""

echo "1. Starting local HTTP servers..."
python3 -m http.server $LOCAL_PORT --directory "$PROJECT_DIR" >/dev/null 2>&1 &
HTTP_PID=$!
python3 -m http.server $LOCAL_PORT2 --directory "$PROJECT_DIR/scripts" >/dev/null 2>&1 &
HTTP2_PID=$!
sleep 1

echo "2. Starting zo-tunnel-server..."
mkdir -p "$CONFIG_DIR/etc/zo-tunnel"
ZO_CONFIG="$CONFIG_DIR/server.yaml"
RUST_LOG=info $SERVER_BIN start \
    --domain "$DOMAIN" \
    --control-port $CONTROL_PORT \
    --public-port $PUBLIC_PORT \
    --token "$TOKEN" \
    --config "$ZO_CONFIG" \
    --force 2>&1 | sed 's/^/   [server] /' &
SERVER_PID=$!
sleep 2

echo "3. Configure zotunnel..."
$CLIENT_BIN config set --server "127.0.0.1:$CONTROL_PORT" --token "$TOKEN"

echo "4. Starting clients..."
RUST_LOG=warn $CLIENT_BIN http "127.0.0.1:$LOCAL_PORT" --name test-app 2>&1 | sed 's/^/   [client-1] /' &
CLIENT_PID=$!
RUST_LOG=warn $CLIENT_BIN http "127.0.0.1:$LOCAL_PORT2" --name api-app 2>&1 | sed 's/^/   [client-2] /' &
CLIENT2_PID=$!
sleep 3

echo "5. Testing subdomain routing..."
RESPONSE=$(curl -s --max-time 10 -H "Host: test-app.$DOMAIN" "http://127.0.0.1:$PUBLIC_PORT/" 2>&1 || echo "CURL_FAILED")
if echo "$RESPONSE" | grep -qi "Cargo.toml\|Directory listing\|<!DOCTYPE"; then
    echo "   OK test-app"
else
    echo "   FAIL test-app: $RESPONSE"
    exit 1
fi

RESPONSE2=$(curl -s --max-time 10 -H "Host: api-app.$DOMAIN" "http://127.0.0.1:$PUBLIC_PORT/" 2>&1 || echo "CURL_FAILED")
if echo "$RESPONSE2" | grep -qi "e2e_test\|build.sh\|install.sh\|Directory listing\|<!DOCTYPE"; then
    echo "   OK api-app"
else
    echo "   FAIL api-app: $RESPONSE2"
    exit 1
fi

DASH=$(curl -s --max-time 5 -H "Host: dashboard.$DOMAIN" "http://127.0.0.1:$PUBLIC_PORT/api/status" 2>&1 || echo "CURL_FAILED")
if echo "$DASH" | grep -q "running\|Authentication required"; then
    echo "   OK dashboard"
else
    echo "   FAIL dashboard: $DASH"
    exit 1
fi

echo ""
echo "E2E PASSED"
