//! "Set up via USB": find the phone on USB, learn its tailnet IP, and switch
//! adbd to TCP mode so we can reach it over the tailnet afterwards.

use std::net::Ipv4Addr;
use std::time::Duration;

use serde::Serialize;

use crate::adb::{Adb, DeviceState};
use crate::error::{AppError, ErrorCode};
use crate::tailnet::{Peer, TailnetSnapshot};

#[derive(Debug, Clone, Serialize)]
pub struct UsbPhone {
    pub serial: String,
    pub model: String,
    pub android_version: String,
    /// The phone's Tailscale IPv4, if Tailscale is running on it.
    pub tailnet_ip: Option<Ipv4Addr>,
}

/// Find exactly one authorized phone on USB.
pub async fn find_usb_phone(adb: &Adb) -> Result<UsbPhone, AppError> {
    adb.start_server().await?;
    let usb = adb.usb_devices().await?;
    let dev = match usb.as_slice() {
        [] => {
            return Err(AppError::new(ErrorCode::NoUsbDevice, "No phone found on USB")
                .hint("Plug the phone in with a data cable, unlock it, and make sure USB debugging is on (Settings → Developer options)."));
        }
        [one] => one.clone(),
        _ => {
            return Err(AppError::new(ErrorCode::MultipleUsbDevices, format!("{} devices are connected over USB", usb.len()))
                .hint("Unplug all but the phone you want to set up."));
        }
    };
    match dev.state {
        DeviceState::Device => {}
        DeviceState::Unauthorized | DeviceState::Authorizing => {
            return Err(AppError::new(ErrorCode::AdbUnauthorized, "The phone hasn't allowed this Mac yet")
                .hint("Unlock the phone and tap \"Allow\" on the \"Allow USB debugging?\" prompt (tick \"Always allow\"), then try again."));
        }
        DeviceState::NoPermissions => {
            return Err(AppError::new(ErrorCode::Adb, "adb has no permission to use this USB device"));
        }
        ref other => {
            return Err(AppError::new(ErrorCode::Adb, format!("The phone is in state {other:?}"))
                .hint("Unplug and replug the cable, then try again."));
        }
    }
    let model = adb.getprop(&dev.serial, "ro.product.model").await.unwrap_or_default();
    let android_version = adb.getprop(&dev.serial, "ro.build.version.release").await.unwrap_or_default();
    let tailnet_ip = adb.tailnet_ipv4(&dev.serial).await.unwrap_or(None);
    Ok(UsbPhone {
        serial: dev.serial,
        model: if model.is_empty() { dev.model.unwrap_or_default().replace('_', " ") } else { model },
        android_version,
        tailnet_ip,
    })
}

/// Restart adbd on the phone listening on `port` (it keeps this until reboot).
pub async fn enable_tcpip(adb: &Adb, usb_serial: &str, port: u16) -> Result<(), AppError> {
    adb.tcpip(usb_serial, port).await?;
    // adbd restarts; give it a moment so the first tailnet dial doesn't race it.
    tokio::time::sleep(Duration::from_secs(2)).await;
    Ok(())
}

/// The tailnet peer that owns `ip`, if any.
pub fn match_peer(snapshot: &TailnetSnapshot, ip: Ipv4Addr) -> Option<Peer> {
    snapshot.peer_by_ip(ip.into()).cloned()
}
