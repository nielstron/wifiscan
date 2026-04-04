# WiFi QR Scanner

Native Rust app for macOS that opens a camera preview, detects standard `WIFI:` QR codes, and connects through `networksetup`.

## Run

```bash
cargo run
```

## Notes

- macOS should prompt for camera permission on first launch.
- The Wi-Fi connection step is macOS-specific.
- Expected QR payload shape:

```text
WIFI:T:WPA;S:MyNetwork;P:secret;;
```
