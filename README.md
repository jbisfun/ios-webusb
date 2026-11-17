# ios-webusb

WebUSB-style access to iPhones and iPads. It runs a userland USB driver and lets web apps reach the device after you approve them. A gpui tray handles the devices and permissions.

- **Driver**: [rusbmux](https://github.com/abdullah-albanna/rusbmux), vendored in `vendor/rusbmux`. It reaches devices over nusb, so the kernel and system `usbmuxd` aren't involved. It serves a usbmuxd-compatible socket at `$XDG_RUNTIME_DIR/ios-webusb/usbmuxd`.
- **Device stack**: Jimpactor's `plume_utils` and `idevice` (path dependency on `~/projs/jimpactor`). They handle discovery, device names and pairing.
- **Web bridge**: `ws://127.0.0.1:27150`. One WebSocket is one usbmuxd connection, and the usbmuxd protocol travels in binary frames. The first time a browser origin connects, the tray asks you to allow it, like WebUSB's device chooser. Approved origins are stored in `~/.config/ios-webusb/origins.json` and can be revoked from the tray. Clients that send no `Origin` header (local programs) are let through.
- **Tray**: a StatusNotifierItem (`ksni`) with devices, Pair, pending requests and allowed sites. Clicking it opens the gpui window.

## Setup

```sh
sudo cp udev/70-ios-webusb.rules /etc/udev/rules.d/ && sudo udevadm control --reload
sudo systemctl mask --now usbmuxd   # only one driver can own the device
cargo run --release                 # --show opens the window at startup
```

Pair records are kept in `~/.local/share/ios-webusb/lockdown`. To point Jimpactor or other usbmuxd clients at this driver, set `USBMUXD_SOCKET_ADDRESS=$XDG_RUNTIME_DIR/ios-webusb/usbmuxd`.

Environment overrides: `IOS_WEBUSB_ADDRESS` (bridge `host:port`) and `IOS_WEBUSB_SOCKET`.

## Web client

`web/` is an npm package (`ios-webusb`, depends on `plist`):

```js
import { Usbmux } from "ios-webusb";
const mux = new Usbmux();                     // ws://127.0.0.1:27150
const [device] = await mux.listDevices();     // { deviceId, udid, connectionType, properties }
const lockdown = await mux.connect(device.deviceId, 62078); // raw TCP tunnel: read(n), write(bytes)
```

## Vendored rusbmux changes

- `idevice` is pinned to `=0.1.68` (Jimpactor's pin) instead of `=0.1.65`.
- The lockdown directory can be set with `rusbmux::handler::set_lockdown_path`, so the daemon runs unprivileged.
