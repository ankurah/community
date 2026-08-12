#!/usr/bin/env bash
# Build the leptos client for the mobile shell and sync it into the iOS
# project. The wasm is compiled with the production websocket URL baked in
# (ws_url() in leptos-app/src/main.rs reads BACKEND_WS_URL at compile time;
# without it the client dials its serving origin, which inside the shell is
# the app bundle, not a server).
set -euo pipefail
cd "$(dirname "$0")"

BACKEND_WS_URL="${BACKEND_WS_URL:-wss://community.ankurah.org}"

echo "==> trunk release build (BACKEND_WS_URL=${BACKEND_WS_URL})"
(cd ../leptos-app && BACKEND_WS_URL="${BACKEND_WS_URL}" trunk build --release)

echo "==> staging web assets into www/"
rm -rf www
cp -R ../leptos-app/dist www

echo "==> capacitor sync"
npx cap sync ios

echo "done — open ios/App/App.xcodeproj or run: npx cap run ios"
