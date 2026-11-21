# FreeMDU for macOS

Native Mac front end for [FreeMDU](https://github.com/medusalix/FreeMDU). It links the official `freemdu` Rust crate directly (no bridge process, no runtime to install) and ships as a self-contained `.app` / `.dmg`. The UI runs in macOS's built-in WebKit.

Requires the FreeMDU adapter flashed in **bridge mode** (appears as `/dev/cu.usbmodem*`).

## Get the app (no Mac toolchain needed)
1. Push this folder to a GitHub repo.
2. Actions → **build-macos** → Run workflow (or push a tag like `v0.1.0`).
3. Download the universal (Apple Silicon + Intel) `.dmg` from the run's artifacts.

## Build locally
```
rustup target add aarch64-apple-darwin x86_64-apple-darwin
cargo install tauri-cli --version "^2" --locked
cargo tauri icon app-icon.png
cargo tauri build --target universal-apple-darwin
```
Output: `src-tauri/target/universal-apple-darwin/release/bundle/`.

## Gatekeeper
Unsigned builds show a warning on other Macs (right-click → Open). For frictionless distribution, sign with a Developer ID certificate and notarize (Apple Developer Program).

## Status
Backend and UI follow the official TUI's behavior (reconnect loop, staggered polling, action argument handling), checked against the FreeMDU source. It depends on FreeMDU's git master, not crates.io 0.1.0, whose API is older. Pin a commit with `rev = "..."` in `src-tauri/Cargo.toml` for reproducible builds.

Not yet compiled or tested against hardware: expect to fix small compile errors on first build.

Licensed MIT OR Apache-2.0, like FreeMDU. Not affiliated with Miele.
