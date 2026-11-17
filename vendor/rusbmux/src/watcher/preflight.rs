#[cfg(not(target_arch = "wasm32"))]
use std::io::ErrorKind;
use std::sync::Arc;

use idevice::{
    IdeviceError, IdeviceService,
    pairing_file::PairingFile,
    provider::IdeviceProvider,
    services::{lockdown::LockdownClient, notification_proxy::NotificationProxyClient},
};
#[cfg(not(target_arch = "wasm32"))]
use tracing::error;
use tracing::{debug, warn};

#[cfg(not(target_arch = "wasm32"))]
use crate::handler::{lockdown_path, read_buid::read_system_buid};
use crate::{device::usb::UsbDevice, error::RusbmuxError, provider::RusbmuxProvider};

const INSECURE_NOTIFICATION_PROXY: &str = "com.apple.mobile.insecure_notification_proxy";
const REQUEST_PAIR: &str = "com.apple.mobile.lockdown.request_pair";
const REQUEST_HOST_BUID: &str = "com.apple.mobile.lockdown.request_host_buid";

#[cfg(not(target_arch = "wasm32"))]
pub(super) fn spawn(device: Arc<UsbDevice>) {
    tokio::spawn(async move {
        let canceler = device.core.canceler.clone();
        tokio::select! {
            Err(err) = run(device) => {
                error!(%err, "Device preflight failed");
            },
            _ = canceler.cancelled() => {}
        }
    });
}

#[cfg(not(target_arch = "wasm32"))]
async fn run(device: Arc<UsbDevice>) -> Result<(), RusbmuxError> {
    let path = lockdown_path().join(format!("{}.plist", device.udid));

    let pairing_file = match tokio::fs::read(&path).await {
        Ok(data) => {
            let pairing_file = PairingFile::from_bytes(&data);

            match pairing_file {
                Ok(p) => Some(p),
                Err(err) => {
                    debug!(%err, "Failed to parse pairing file");
                    tokio::fs::remove_file(&path).await?;
                    None
                }
            }
        }
        Err(err) if err.kind() == ErrorKind::NotFound => None,
        Err(err) => return Err(err.into()),
    };

    let pairing_file = preflight(device, pairing_file).await?;

    if let Some(pf) = pairing_file {
        tokio::fs::write(path, pf.serialize()?).await?;
    }

    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
pub async fn preflight(
    device: Arc<UsbDevice>,
    pairing_file: Option<PairingFile>,
) -> Result<Option<PairingFile>, RusbmuxError> {
    let system_buid = read_system_buid().await?;
    preflight_with_buid(device, pairing_file, system_buid).await
}

/// Validates `pairing_file`, or pairs (waiting for the Trust dialog) as the host
/// identified by `system_buid`. Returns the new pairing file when one was created.
pub async fn preflight_with_buid(
    device: Arc<UsbDevice>,
    pairing_file: Option<PairingFile>,
    system_buid: String,
) -> Result<Option<PairingFile>, RusbmuxError> {
    let provider = RusbmuxProvider::new(device, "rusbmux-preflight".to_string());

    let mut lockdown = LockdownClient::connect(&provider).await?;

    if lockdown.idevice.get_type().await? != "com.apple.mobile.lockdown" {
        // restore mode
        return Ok(None);
    }

    if let Some(pairing_file) = pairing_file {
        match lockdown.start_session(&pairing_file).await {
            Ok(_) => return Ok(None),
            Err(IdeviceError::InvalidHostID) => {}
            // something is wrong with the pairing file
            Err(
                err @ (IdeviceError::Rustls(_)
                | IdeviceError::TlsBuilderFailed(_)
                | IdeviceError::PemParseFailed(_)),
            ) => {
                debug!(
                    %err,
                    "Failed to start a session, the pairing file might be corrupted"
                );
                lockdown = LockdownClient::connect(&provider).await?;
            }
            Err(err) => return Err(err.into()),
        }
    }

    let host_id = uuid::Uuid::new_v4().to_string().to_uppercase();

    lockdown
        .set_value("UntrustedHostBUID", system_buid.as_str().into(), None)
        .await?;

    let product_version = lockdown
        .get_value(Some("ProductVersion"), None)
        .await?
        .into_string()
        .ok_or(RusbmuxError::InvalidData(
            "`ProductVersion` is not a string",
        ))?;

    let product_version_major = product_version
        .split('.')
        .next()
        .and_then(|v| v.parse::<u32>().ok())
        .ok_or(RusbmuxError::InvalidData("`ProductVersion` is not valid"))?;

    let device_class = lockdown
        .get_value(Some("DeviceClass"), None)
        .await?
        .into_string()
        .ok_or(RusbmuxError::InvalidData("`DeviceClass` is not a string"))?;

    let support_notifications = (matches!(device_class.as_str(), "iPhone" | "iPad")
        && product_version_major >= 7)
        || (device_class == "Watch" && product_version_major >= 2)
        || (device_class == "AppleTV" && product_version_major >= 9);

    // TODO: test on an old device
    let pairing_file = match lockdown.pair_once(&host_id, &system_buid, None).await {
        Ok(pairing_file) => pairing_file,
        Err(
            err @ (IdeviceError::PasswordProtected | IdeviceError::PairingDialogResponsePending),
        ) => {
            if support_notifications {
                let mut notifications = connect_notifications(&provider, &mut lockdown).await?;

                wait_for_pair_request(&mut lockdown, &mut notifications, &system_buid, &host_id)
                    .await?;
                lockdown.pair_once(host_id, system_buid, None).await?
            } else {
                warn!(%err, "Failed to pair");
                warn!("Device doesn't support notification proxy, trust the host and retry again");
                return Ok(None);
            }
        }
        Err(err) => return Err(err.into()),
    };

    debug!("Verifying the pairing file");
    lockdown.start_session(&pairing_file).await?;

    // TODO: ValidatePair

    debug!("Preflight succeeded");

    Ok(Some(pairing_file))
}

async fn connect_notifications(
    provider: &RusbmuxProvider,
    lockdown: &mut LockdownClient,
) -> Result<NotificationProxyClient, RusbmuxError> {
    let (port, _) = lockdown.start_service(INSECURE_NOTIFICATION_PROXY).await?;
    let mut notifications = NotificationProxyClient::new(provider.connect(port).await?);

    notifications
        .observe_notifications(&[REQUEST_PAIR, REQUEST_HOST_BUID])
        .await?;

    Ok(notifications)
}

async fn wait_for_pair_request(
    lockdown: &mut LockdownClient,
    notifications: &mut NotificationProxyClient,
    system_buid: &str,
    host_id: &str,
) -> Result<(), RusbmuxError> {
    loop {
        tokio::select! {
            notification = notifications.receive_notification() => {
                match notification?.as_str() {
                    REQUEST_PAIR => return Ok(()),
                    REQUEST_HOST_BUID => {
                        lockdown
                            .set_value("UntrustedHostBUID", system_buid.into(), None)
                            .await?;
                    }
                    _ => {}

                }
            },
            // break the preflight task if the pairing got denied
            //
            // TODO: is this a problem?
            _ = crate::rt::sleep(std::time::Duration::from_secs(30)) => {
                if let Err(err @ IdeviceError::UserDeniedPairing) = lockdown.pair_once(host_id, system_buid, None).await {
                    debug!("User denied pairing, cancelling preflight");
                    break Err(RusbmuxError::Idevice(err));
                }
            }
        }
    }
}
