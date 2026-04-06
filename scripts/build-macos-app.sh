#!/bin/zsh
set -euo pipefail

cd "$(dirname "$0")/.."

app_name="WiFi QR Scanner"
bundle_id="com.niels.wifiscan"
binary_name="wifiscan"
bundle_dir="target/release/bundle/osx/${app_name}.app"
contents_dir="${bundle_dir}/Contents"
macos_dir="${contents_dir}/MacOS"
resources_dir="${contents_dir}/Resources"

version="$(python3 - <<'PY'
import tomllib
from pathlib import Path
data = tomllib.loads(Path('Cargo.toml').read_text())
print(data['package']['version'])
PY
)"

cargo build --release --bin "${binary_name}"

rm -rf "${bundle_dir}"
mkdir -p "${macos_dir}" "${resources_dir}"

cp "target/release/${binary_name}" "${macos_dir}/${binary_name}"
cp "assets/AppIcon.icns" "${resources_dir}/AppIcon.icns"

cat > "${contents_dir}/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>English</string>
    <key>CFBundleDisplayName</key>
    <string>${app_name}</string>
    <key>CFBundleExecutable</key>
    <string>${binary_name}</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon.icns</string>
    <key>CFBundleIdentifier</key>
    <string>${bundle_id}</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>${app_name}</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>${version}</string>
    <key>CFBundleVersion</key>
    <string>${version}</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.utilities</string>
    <key>LSUIElement</key>
    <true/>
    <key>NSCameraUsageDescription</key>
    <string>WiFi QR Scanner needs camera access to scan Wi-Fi QR codes and connect you to the detected network.</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
EOF

if command -v codesign >/dev/null 2>&1; then
    codesign --force --deep --sign - "${bundle_dir}" >/dev/null 2>&1 || true
fi

echo "${bundle_dir}"
