# WebUSB driver (wasm) -> web/pkg
web:
    cargo build -p ios-webusb-web --target wasm32-unknown-unknown --release
    wasm-bindgen --target web --out-dir web/pkg target/wasm32-unknown-unknown/release/ios_webusb_web.wasm

# Demo page on http://localhost:8000 (WebUSB needs a secure context; localhost counts)
serve: web
    python3 -m http.server -d web 8000

# Native driver + tray
tray *args:
    cargo run --release -p ios-webusb -- {{args}}
