//! A WebUSB driver for iPhones and iPads. The page owns the device through
//! `navigator.usb`; rusbmux speaks the usbmux protocol over its bulk endpoints and
//! idevice runs lockdown on top. Nothing native is involved.

#![cfg(target_arch = "wasm32")]

use std::cell::RefCell;
use std::sync::Arc;

use idevice::pairing_file::PairingFile;
use idevice::provider::IdeviceProvider;
use idevice::services::lockdown::LockdownClient;
use idevice::{Idevice, IdeviceService};
use rusbmux::device::usb::UsbDevice;
use rusbmux::provider::RusbmuxProvider;
use rusbmux::usb_backend::{APPLE_VID, AnyDeviceInfo, next_device_id};
use wasm_bindgen::prelude::*;

const LABEL: &str = "ios-webusb";

#[wasm_bindgen(start)]
fn start() {
    console_error_panic_hook::set_once();
    tracing_wasm::set_as_global_default();
}

fn js_error(error: impl std::fmt::Display) -> JsError {
    JsError::new(&error.to_string())
}

/// Plist dictionaries become plain objects, so results work with `JSON.stringify`.
fn to_js(value: &plist::Value) -> Result<JsValue, JsError> {
    use serde::Serialize;
    value
        .serialize(&serde_wasm_bindgen::Serializer::json_compatible())
        .map_err(js_error)
}

/// An iOS device the user granted to this page.
#[wasm_bindgen]
pub struct IosDevice {
    device: Arc<UsbDevice>,
    pairing: RefCell<Option<PairingFile>>,
}

#[wasm_bindgen]
impl IosDevice {
    /// Shows the browser's device chooser. Must run from a user gesture (e.g. a click).
    pub async fn request() -> Result<Option<IosDevice>, JsError> {
        let selector = nusb::DeviceSelector::all().with_vid(APPLE_VID);
        match nusb::request_device(&[selector]).await.map_err(js_error)? {
            Some(info) => Ok(Some(Self::open(info).await?)),
            None => Ok(None),
        }
    }

    /// Devices this origin was already granted, without prompting.
    pub async fn authorized() -> Result<Vec<IosDevice>, JsError> {
        let mut devices = Vec::new();
        for info in nusb::list_devices().await.map_err(js_error)? {
            if info.vendor_id() == APPLE_VID {
                devices.push(Self::open(info).await?);
            }
        }
        Ok(devices)
    }

    async fn open(info: nusb::DeviceInfo) -> Result<IosDevice, JsError> {
        let device = UsbDevice::new(AnyDeviceInfo::Nusb(info), next_device_id())
            .await
            .map_err(js_error)?;
        Ok(IosDevice {
            device,
            pairing: RefCell::new(None),
        })
    }

    #[wasm_bindgen(getter)]
    pub fn udid(&self) -> String {
        self.device.udid.clone()
    }

    fn provider(&self) -> RusbmuxProvider {
        let mut provider = RusbmuxProvider::new(Arc::clone(&self.device), LABEL.into());
        if let Some(pairing) = self.pairing.borrow().clone() {
            provider.set_pairing_file(pairing);
        }
        provider
    }

    /// Uses `pairRecord` if the device still trusts it, otherwise pairs as the host
    /// `systemBuid` (the device shows "Trust This Computer?"). Returns the new pair
    /// record when one was created; store it and pass it next time.
    #[wasm_bindgen(js_name = pair)]
    pub async fn pair(
        &self,
        #[wasm_bindgen(js_name = pairRecord)] pair_record: Option<Vec<u8>>,
        #[wasm_bindgen(js_name = systemBuid)] system_buid: String,
    ) -> Result<Option<Vec<u8>>, JsError> {
        if let Some(bytes) = pair_record {
            let pairing = PairingFile::from_bytes(&bytes).map_err(js_error)?;
            self.pairing.replace(Some(pairing));
        }
        let mut provider = self.provider();
        if !provider
            .preflight_with_buid(system_buid)
            .await
            .map_err(js_error)?
        {
            return Ok(None);
        }
        let pairing = provider.get_pairing_file().await.map_err(js_error)?;
        self.pairing.replace(Some(pairing.clone()));
        pairing.serialize().map(Some).map_err(js_error)
    }

    /// Reads a lockdown value (e.g. `"DeviceName"`, `"ProductVersion"`), or every value
    /// when `key` is omitted. Values beyond the basic identity need `pair` first.
    #[wasm_bindgen(js_name = getValue)]
    pub async fn get_value(
        &self,
        key: Option<String>,
        domain: Option<String>,
    ) -> Result<JsValue, JsError> {
        let provider = self.provider();
        let mut lockdown = LockdownClient::connect(&provider).await.map_err(js_error)?;
        let pairing = self.pairing.borrow().clone();
        if let Some(pairing) = pairing {
            lockdown.start_session(&pairing).await.map_err(js_error)?;
        }
        let value = lockdown
            .get_value(key.as_deref(), domain.as_deref())
            .await
            .map_err(js_error)?;
        to_js(&value)
    }

    /// Starts a lockdown service (e.g. `"com.apple.afc"`) and returns its stream,
    /// with TLS already set up when the service asks for it. Requires `pair`.
    #[wasm_bindgen(js_name = startService)]
    pub async fn start_service(&self, name: String) -> Result<DeviceStream, JsError> {
        let pairing = self
            .pairing
            .borrow()
            .clone()
            .ok_or_else(|| JsError::new("not paired: call pair() first"))?;
        let provider = self.provider();
        let mut lockdown = LockdownClient::connect(&provider).await.map_err(js_error)?;
        let legacy = lockdown.start_session(&pairing).await.map_err(js_error)?;
        let (port, ssl) = lockdown.start_service(name).await.map_err(js_error)?;
        let mut idevice = provider.connect(port).await.map_err(js_error)?;
        if ssl {
            idevice
                .start_session(&pairing, legacy)
                .await
                .map_err(js_error)?;
        }
        Ok(DeviceStream(idevice))
    }

    /// Opens a raw TCP connection to `port` on the device (62078 is lockdownd).
    pub async fn connect(&self, port: u16) -> Result<DeviceStream, JsError> {
        let idevice = self.provider().connect(port).await.map_err(js_error)?;
        Ok(DeviceStream(idevice))
    }

    /// Closes every connection and releases the USB interface.
    pub async fn close(&self) -> Result<(), JsError> {
        self.device.shutdown().await.map_err(js_error)
    }
}

/// A byte stream to a port or service on the device.
#[wasm_bindgen]
pub struct DeviceStream(Idevice);

#[wasm_bindgen]
impl DeviceStream {
    /// Reads up to `max` bytes (at least one; resolves when data arrives).
    pub async fn read(&mut self, max: u32) -> Result<Vec<u8>, JsError> {
        self.0.read_any(max).await.map_err(js_error)
    }

    /// Reads exactly `length` bytes.
    #[wasm_bindgen(js_name = readExact)]
    pub async fn read_exact(&mut self, length: u32) -> Result<Vec<u8>, JsError> {
        self.0.read_raw(length as usize).await.map_err(js_error)
    }

    pub async fn write(&mut self, data: Vec<u8>) -> Result<(), JsError> {
        self.0.send_raw(&data).await.map_err(js_error)
    }

    /// Sends a plist message with lockdown/service framing (big-endian u32 length + XML).
    #[wasm_bindgen(js_name = sendPlist)]
    pub async fn send_plist(&mut self, message: JsValue) -> Result<(), JsError> {
        let value: plist::Value = serde_wasm_bindgen::from_value(message).map_err(js_error)?;
        let mut body = Vec::new();
        value.to_writer_xml(&mut body).map_err(js_error)?;
        let length = u32::try_from(body.len()).map_err(js_error)?;
        let mut frame = length.to_be_bytes().to_vec();
        frame.extend(body);
        self.0.send_raw(&frame).await.map_err(js_error)
    }

    /// Receives one length-framed plist message.
    #[wasm_bindgen(js_name = readPlist)]
    pub async fn read_plist(&mut self) -> Result<JsValue, JsError> {
        let header = self.0.read_raw(4).await.map_err(js_error)?;
        let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        let body = self.0.read_raw(length as usize).await.map_err(js_error)?;
        let value = plist::Value::from_reader(std::io::Cursor::new(body)).map_err(js_error)?;
        to_js(&value)
    }
}
