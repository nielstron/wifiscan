# WiFi QR Scanner

Native Rust app for macOS that opens a camera preview, detects standard `WIFI:` QR codes, and connects through `networksetup`.

## Run

```bash
cargo run
```

## Build App Bundle

```bash
zsh ./scripts/build-macos-app.sh
```

The generated app bundle will be at:

```text
target/release/bundle/osx/WiFi QR Scanner.app
```

To install it into `~/Applications`:

```bash
zsh ./scripts/install-macos-app.sh
```

## Notes

- macOS should prompt for camera permission on first launch.
- The Wi-Fi connection step is macOS-specific.
- Expected QR payload shape:

```text
WIFI:T:WPA;S:MyNetwork;P:secret;;
```
