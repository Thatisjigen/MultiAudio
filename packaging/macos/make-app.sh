#!/bin/sh
# Builds dist/MultiAudio.app (release build, ad-hoc signed).
# The Info.plist declares system-audio capture (Core Audio tap, no driver
# needed) and microphone use (auto-calibration).
set -e
cd "$(dirname "$0")/../.."
# (a shell that did not read the user's profile has no cargo in its PATH)
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
# --universal: one binary for Intel and Apple Silicon Macs
if [ "$1" = "--universal" ]; then
  rustup target add x86_64-apple-darwin aarch64-apple-darwin
  cargo build --release --target x86_64-apple-darwin
  cargo build --release --target aarch64-apple-darwin
  mkdir -p target/universal
  lipo -create -output target/universal/multiaudio \
    target/x86_64-apple-darwin/release/multiaudio target/aarch64-apple-darwin/release/multiaudio
  BIN=target/universal/multiaudio
else
  cargo build --release
  BIN=target/release/multiaudio
fi
APP=dist/MultiAudio.app
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
# A copy that is running keeps running the old program, and "open" would
# only bring it to the front: ask it to quit first (it puts the system
# output back as it was), and wait for it.
if pgrep -x MultiAudio >/dev/null 2>&1; then
  echo "MultiAudio è aperta: la chiudo per sostituirla"
  pkill -INT -x MultiAudio
  n=0
  while pgrep -x MultiAudio >/dev/null 2>&1 && [ "$n" -lt 100 ]; do
    sleep 0.1
    n=$((n + 1))
  done
  if pgrep -x MultiAudio >/dev/null 2>&1; then
    echo "Non si è chiusa: chiudila a mano e riprova" >&2
    exit 1
  fi
fi
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/MultiAudio"
cp assets/MultiAudio.icns "$APP/Contents/Resources/MultiAudio.icns"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>MultiAudio</string>
  <key>CFBundleDisplayName</key><string>MultiAudio</string>
  <key>CFBundleIdentifier</key><string>app.multiaudio.MultiAudio</string>
  <key>CFBundleExecutable</key><string>MultiAudio</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleIconFile</key><string>MultiAudio</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSAudioCaptureUsageDescription</key><string>MultiAudio cattura l'audio del sistema per mandarlo a più uscite insieme, ognuna con il ritardo giusto.</string>
  <key>NSMicrophoneUsageDescription</key><string>MultiAudio usa il microfono solo durante la calibrazione automatica dei ritardi.</string>
</dict>
</plist>
PLIST
# Ad-hoc signature with a designated requirement based on the bundle id, so
# macOS keeps the granted permissions (system audio, microphone) across
# rebuilds instead of asking again every time. For public releases sign with
# a Developer ID instead.
codesign --force --deep --sign - -r='designated => identifier "app.multiaudio.MultiAudio"' "$APP" >/dev/null 2>&1 || true
echo "Creato $APP"
