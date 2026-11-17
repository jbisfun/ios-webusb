# ios-webusb

A WebUSB driver for iPhones and iPads. The web page claims the device through `navigator.usb` and runs the driver itself in wasm: [rusbmux](https://github.com/abdullah-albanna/rusbmux) (vendored, ported to WebUSB) for the usbmux protocol, and [idevice](https://github.com/jkcoxson/idevice) for lockdown, pairing and services. No daemon, no extension, nothing native. Works in Chromium-based browsers.

There's also a native build of the same driver with a gpui tray, for local tools.

## Web

```sh
just web     # -> web/pkg (wasm-bindgen --target web)
just serve   # demo page on http://localhost:8000
```

```js
import init, { IosDevice } from "./pkg/ios_webusb_web.js";
await init();

const device = await IosDevice.request();          // browser chooser; call from a click
// or: const [device] = await IosDevice.authorized(); // devices granted before

// The page is the host: keep one SystemBUID and the pair record per device.
const record = await device.pair(savedRecord, systemBuid); // "Trust This Computer?" on first use
await device.getValue("ProductVersion");

const afc = await device.startService("com.apple.afc");    // TLS set up when needed
await afc.write(bytes); const reply = await afc.read(65536);
const lockdownd = await device.connect(62078);             // raw TCP; sendPlist/readPlist frame plists
```

Linux needs the udev rule so the browser can open the device, and nothing else may hold the device:

```sh
sudo cp udev/70-ios-webusb.rules /etc/udev/rules.d/ && sudo udevadm control --reload
sudo systemctl mask --now usbmuxd
```

## Native tray

`just tray` (`--show` opens the window). It serves a usbmuxd socket at `$XDG_RUNTIME_DIR/ios-webusb/usbmuxd` (point tools at it with `USBMUXD_SOCKET_ADDRESS`), lists devices, pairs them, and can **release** a device so a web page can claim it. Only one driver can own a device at a time. Pair records are stored in `~/.local/share/ios-webusb/lockdown`.

## Vendored rusbmux changes

- Builds for `wasm32-unknown-unknown` with nusb's WebUSB backend. On that target, the daemon, Wi-Fi and filesystem parts are compiled out. Tasks run on the page's event loop (`rt.rs`). WebUSB handles are marked `Send` because wasm without atomics is single-threaded.
- `preflight_with_buid` lets the host supply its own SystemBUID.
- `idevice` is pinned to `=0.1.68` (Jimpactor's pin).
- The lockdown directory can be set with `rusbmux::handler::set_lockdown_path`, so the native daemon runs unprivileged.
