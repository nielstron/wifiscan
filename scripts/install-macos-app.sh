#!/bin/zsh
set -euo pipefail

cd "$(dirname "$0")/.."
zsh ./scripts/build-macos-app.sh

app_path="target/release/bundle/osx/WiFi QR Scanner.app"
install_dir="${HOME}/Applications"
mkdir -p "$install_dir"
rm -rf "${install_dir}/WiFi QR Scanner.app"
cp -R "$app_path" "$install_dir/"
echo "Installed to ${install_dir}/WiFi QR Scanner.app"
