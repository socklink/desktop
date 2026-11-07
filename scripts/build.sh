#!/bin/bash
set -e

# Extract current version
CURRENT_VERSION=$(node -e "console.log(require('./src-tauri/tauri.conf.json').version)")

# Prompt for new version
printf "Enter new version (current is %s): " "$CURRENT_VERSION"
read VERSION

if [ -z "$VERSION" ]; then
  echo "No version entered. Keeping current version ($CURRENT_VERSION)."
  VERSION=$CURRENT_VERSION
else
  # Update version in tauri.conf.json
  echo "Updating tauri.conf.json to version $VERSION..."
  node -e "
  const fs = require('fs');
  const file = './src-tauri/tauri.conf.json';
  const data = JSON.parse(fs.readFileSync(file, 'utf8'));
  data.version = '$VERSION';
  fs.writeFileSync(file, JSON.stringify(data, null, 2), 'utf8');
  "
fi

echo "Select OS to build:"
echo "1) Linux (Debian)"
echo "2) Windows"
echo "3) Both (Default)"
printf "Choice [1-3]: "
read BUILD_CHOICE

BUILD_LINUX=false
BUILD_WINDOWS=false

case "$BUILD_CHOICE" in
  1)
    BUILD_LINUX=true
    ;;
  2)
    BUILD_WINDOWS=true
    ;;
  *)
    BUILD_LINUX=true
    BUILD_WINDOWS=true
    ;;
esac

# 1. Run the build scripts
if [ "$BUILD_LINUX" = true ]; then
  echo "Building Debian package..."
  sh ./scripts/build-deb.sh
fi

if [ "$BUILD_WINDOWS" = true ]; then
  echo "Building Windows setup..."
  sh ./scripts/build-win.sh
fi

echo "Building version: $VERSION"

# 3. Define source and destination paths
DEB_SRC="src-tauri/target/release/bundle/deb/SockLink_${VERSION}_amd64.deb"
DEB_SIG_SRC="${DEB_SRC}.sig"
WIN_SRC="src-tauri/target/x86_64-pc-windows-gnu/release/bundle/nsis/SockLink_${VERSION}_x64-setup.exe"
WIN_SIG_SRC="${WIN_SRC}.sig"

DEB_DEST="release/deb/app.deb"
DEB_SIG_DEST="release/deb/app.deb.sig"
WIN_DEST="release/windows/app.exe"
WIN_SIG_DEST="release/windows/app.exe.sig"

# 4. Ensure release directories exist
mkdir -p release/deb release/windows

# 5. Copy the artifacts
echo "Copying built artifacts to release/..."
if [ "$BUILD_LINUX" = true ]; then
  cp "$DEB_SRC" "$DEB_DEST"
  cp "$DEB_SIG_SRC" "$DEB_SIG_DEST"
fi
if [ "$BUILD_WINDOWS" = true ]; then
  cp "$WIN_SRC" "$WIN_DEST"
  cp "$WIN_SIG_SRC" "$WIN_SIG_DEST"
fi

# Copy to showcase public folder for downloading
SHOWCASE_DOWNLOADS="../showcase/public/downloads"
if [ -d "$SHOWCASE_DOWNLOADS" ] || mkdir -p "$SHOWCASE_DOWNLOADS"; then
  echo "Copying to showcase downloads..."
  if [ "$BUILD_LINUX" = true ]; then
    cp "$DEB_DEST" "$SHOWCASE_DOWNLOADS/app.deb"
  fi
  if [ "$BUILD_WINDOWS" = true ]; then
    cp "$WIN_DEST" "$SHOWCASE_DOWNLOADS/app.exe"
  fi
  
  # Copy Android APK if it exists
  APK_SRC="../app/app/build/intermediates/apk/debug/app-debug.apk"
  if [ -f "$APK_SRC" ]; then
    cp "$APK_SRC" "$SHOWCASE_DOWNLOADS/app.apk"
  fi
fi

# Copy the public key for reference
if [ -f "/home/adhil/.tauri/myapp.key.pub" ]; then
  cp "/home/adhil/.tauri/myapp.key.pub" "release/public.key"
fi

# 6. Generate latest.json
PUB_DATE=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

cat <<EOF > release/latest.json
{
  "version": "$VERSION",
  "notes": "Release v$VERSION",
  "pub_date": "$PUB_DATE",
  "platforms": {
$(if [ "$BUILD_LINUX" = true ]; then
DEB_SIG=$(cat "$DEB_SIG_DEST")
cat <<INNER_EOF
    "linux-x86_64": {
      "signature": "$DEB_SIG",
      "url": "https://github.com/pzynk/desktop/releases/download/v$VERSION/SockLink_${VERSION}_amd64.deb"
    }$(if [ "$BUILD_WINDOWS" = true ]; then echo ","; else echo ""; fi)
INNER_EOF
fi)
$(if [ "$BUILD_WINDOWS" = true ]; then
WIN_SIG=$(cat "$WIN_SIG_DEST")
cat <<INNER_EOF
    "windows-x86_64": {
      "signature": "$WIN_SIG",
      "url": "https://github.com/pzynk/desktop/releases/download/v$VERSION/SockLink_${VERSION}_x64-setup.exe"
    }
INNER_EOF
fi)
  }
}
EOF

# 7. Git commit and push latest.json
echo "Committing and pushing latest.json to GitHub..."
git add release/latest.json
git commit -m "chore: release v$VERSION"
git push origin main

# 8. Create GitHub Release and upload binaries
echo "Creating GitHub Release v$VERSION and uploading binaries..."
# Clear positional parameters to build the upload arguments list in a POSIX-compliant way
set --
if [ "$BUILD_LINUX" = true ]; then
  set -- "$@" "$DEB_SRC"
fi
if [ "$BUILD_WINDOWS" = true ]; then
  set -- "$@" "$WIN_SRC"
fi
gh release create "v$VERSION" "$@" --title "v$VERSION" --notes "Release v$VERSION"

echo "Done! Release v$VERSION successfully built, pushed, and published on GitHub!"
