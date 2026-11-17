pub mod core;
#[cfg(not(target_arch = "wasm32"))]
pub mod network;
pub mod packet_router;
#[cfg(not(target_arch = "wasm32"))]
pub mod power_assertion;
pub mod usb;
use std::{borrow::Cow, sync::Arc};

#[cfg(not(target_arch = "wasm32"))]
use network::NetworkDevice;
#[cfg(not(target_arch = "wasm32"))]
use std::net::IpAddr;
use usb::UsbDevice;

use crate::{conn::DeviceConn, error::RusbmuxError, usb_backend::AnyDeviceInfo};

#[derive(Debug)]
pub enum Device {
    #[cfg(not(target_arch = "wasm32"))]
    Network(NetworkDevice),
    Usb(Arc<UsbDevice>),
}

#[derive(Debug)]
pub enum ConnectionType {
    Usb,
    Network,
}

impl Device {
    #[must_use]
    pub const fn connection_type(&self) -> ConnectionType {
        match self {
            Self::Usb(_) => ConnectionType::Usb,
            #[cfg(not(target_arch = "wasm32"))]
            Self::Network(_) => ConnectionType::Network,
        }
    }

    #[must_use]
    pub fn id(&self) -> u64 {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Self::Network(dev) => dev.core.id,
            Self::Usb(dev) => dev.core.id,
        }
    }

    pub fn udid(&self) -> Cow<'_, str> {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Self::Network(dev) => Cow::Borrowed(&dev.udid),
            Self::Usb(dev) => Cow::Borrowed(&dev.udid),
        }
    }

    pub async fn new_usb(info: AnyDeviceInfo, id: u64) -> Result<Self, RusbmuxError> {
        Ok(Self::Usb(UsbDevice::new(info, id).await?))
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub async fn new_network(
        id: u64,
        addr: (IpAddr, Option<IpAddr>),
        scope_id: Option<u32>,
        mac_address: String,
        service_name: String,
        udid: String,
    ) -> Result<Self, RusbmuxError> {
        Ok(Self::Network(
            NetworkDevice::new(id, addr, scope_id, mac_address, service_name, udid).await?,
        ))
    }

    pub async fn shutdown(&self) -> Result<(), RusbmuxError> {
        match self {
            Self::Usb(dev) => dev.shutdown().await?,
            #[cfg(not(target_arch = "wasm32"))]
            Self::Network(dev) => dev.shutdown(),
        }

        Ok(())
    }

    pub async fn connect(&self, destination_port: u16) -> Result<DeviceConn, RusbmuxError> {
        match self {
            Self::Usb(dev) => dev.connect(destination_port).await.map(DeviceConn::Usb),
            #[cfg(not(target_arch = "wasm32"))]
            Self::Network(dev) => dev.connect(destination_port).await.map(DeviceConn::Network),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[must_use]
    pub const fn as_network(&self) -> Option<&NetworkDevice> {
        if let Self::Network(dev) = self {
            Some(dev)
        } else {
            None
        }
    }

    #[must_use]
    #[cfg_attr(target_arch = "wasm32", allow(irrefutable_let_patterns))]
    pub const fn as_usb(&self) -> Option<&Arc<UsbDevice>> {
        if let Self::Usb(dev) = self {
            Some(dev)
        } else {
            None
        }
    }

    pub fn create_device_attached(&self) -> Result<plist::Value, RusbmuxError> {
        match self {
            Self::Usb(dev) => dev.create_device_attached(),
            #[cfg(not(target_arch = "wasm32"))]
            Self::Network(dev) => dev.create_device_attached(),
        }
    }
}
