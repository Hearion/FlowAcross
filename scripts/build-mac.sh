#!/usr/bin/env bash
# 打包 macOS 安装包（release 版 .app + .dmg），复制到 share/。
#
# 本机有「FlowAcross Dev」证书（scripts/setup-mac-signing.sh 创建）时用它签名，
# 这样重新打包后系统授权仍然有效；没有时退回 ad-hoc 签名，每次安装都要重新授权。
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

IDENTITY="FlowAcross Dev"
if security find-identity -p codesigning | grep -q "\"$IDENTITY\""; then
  export APPLE_SIGNING_IDENTITY="$IDENTITY"
  echo "使用证书「$IDENTITY」签名。"
else
  echo "未找到证书「$IDENTITY」，使用 ad-hoc 签名（每次安装都要重新授权）。运行 scripts/setup-mac-signing.sh 可创建。"
fi

cd "$ROOT/app"
npx tauri build --bundles app,dmg

APP="$ROOT/target/release/bundle/macos/FlowAcross.app"
DMG="$(ls -t "$ROOT"/target/release/bundle/dmg/*.dmg | head -1)"
mkdir -p "$ROOT/share"
cp "$DMG" "$ROOT/share/"
echo
codesign -d -r- "$APP" 2>&1 | grep designated || true
echo "安装包：share/$(basename "$DMG")"
