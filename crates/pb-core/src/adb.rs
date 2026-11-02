//! Thin async wrapper around the bundled `adb`, always talking to our private
//! adb server (see `BundledTools::adb_server_port`).

use std::net::Ipv4Addr;
use std::process::Stdio;
use std::time::Duration;

use serde::Serialize;
use tokio::process::Command;

use crate::error::{AppError, ErrorCode};
use crate::tools::BundledTools;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Usb,
    Tcp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceState {
    Device,
    Offline,
    Unauthorized,
    Authorizing,
    Connecting,
    NoPermissions,
    Other(String),
}

impl DeviceState {
    fn parse(s: &str) -> Self {
        match s {
            "device" => Self::Device,
            "offline" => Self::Offline,
            "unauthorized" => Self::Unauthorized,
            "authorizing" => Self::Authorizing,
            "connecting" => Self::Connecting,
            other => Self::Other(other.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Device {
    pub serial: String,
    pub state: DeviceState,
    pub transport: Transport,
    pub model: Option<String>,
    pub product: Option<String>,
}

/// Parse `adb devices -l`.
pub fn parse_devices(text: &str) -> Vec<Device> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty()
            || line.starts_with("List of devices")
            || line.starts_with('*')
            || line.starts_with("adb ")
        {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(serial) = parts.next() else { continue };
        let rest: Vec<&str> = parts.collect();
        let state = if line.contains("no permissions") {
            DeviceState::NoPermissions
        } else {
            match rest.first() {
                Some(s) => DeviceState::parse(s),
                None => continue,
            }
        };
        let field = |k: &str| {
            rest.iter()
                .find_map(|p| p.strip_prefix(k).and_then(|v| v.strip_prefix(':')))
                .map(str::to_string)
        };
        let is_usb = field("usb").is_some()
            || !(serial.contains(':')
                || serial.starts_with("emulator-")
                || serial.contains("._adb-tls-"));
        out.push(Device {
            serial: serial.to_string(),
            state,
            transport: if is_usb {
                Transport::Usb
            } else {
                Transport::Tcp
            },
            model: field("model"),
            product: field("product"),
        });
    }
    out
}

/// Pick the phone's tailnet address (100.64.0.0/10) from `ip -4 -o addr` output.
pub fn parse_tailnet_ipv4(text: &str) -> Option<Ipv4Addr> {
    text.split_whitespace()
        .filter_map(|tok| tok.split('/').next()?.parse::<Ipv4Addr>().ok())
        .find(|ip| {
            let o = ip.octets();
            o[0] == 100 && (o[1] & 0xC0) == 64
        })
}

#[derive(Debug, Clone)]
pub struct AdbOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

impl AdbOutput {
    pub fn text(&self) -> String {
        format!("{}{}", self.stdout, self.stderr).trim().to_string()
    }
}

#[derive(Debug, Clone)]
pub struct Adb {
    tools: BundledTools,
}

impl Adb {
    pub fn new(tools: BundledTools) -> Self {
        Self { tools }
    }

    pub fn tools(&self) -> &BundledTools {
        &self.tools
    }

    pub fn command(&self) -> Command {
        let mut cmd = Command::new(&self.tools.adb);
        self.tools.apply_env(&mut cmd);
        cmd.stdin(Stdio::null()).kill_on_drop(true);
        cmd
    }

    /// Run adb with a timeout; returns output even on non-zero exit.
    pub async fn run(&self, args: &[&str], timeout: Duration) -> Result<AdbOutput, AppError> {
        let mut cmd = self.command();
        cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
        let child = cmd.spawn().map_err(|e| {
            AppError::new(
                ErrorCode::MissingBinary,
                format!(
                    "could not run bundled adb ({}): {e}",
                    self.tools.adb.display()
                ),
            )
        })?;
        let out = tokio::time::timeout(timeout, child.wait_with_output())
            .await
            .map_err(|_| {
                AppError::new(
                    ErrorCode::Adb,
                    format!(
                        "`adb {}` timed out after {}s",
                        args.join(" "),
                        timeout.as_secs()
                    ),
                )
            })?
            .map_err(AppError::from)?;
        Ok(AdbOutput {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    async fn run_ok(&self, args: &[&str], timeout: Duration) -> Result<String, AppError> {
        let out = self.run(args, timeout).await?;
        if out.success {
            Ok(out.stdout)
        } else {
            Err(AppError::new(
                ErrorCode::Adb,
                format!("`adb {}` failed: {}", args.join(" "), out.text()),
            ))
        }
    }

    pub async fn start_server(&self) -> Result<(), AppError> {
        self.run_ok(&["start-server"], Duration::from_secs(20))
            .await
            .map(|_| ())
    }

    pub async fn kill_server(&self) -> Result<(), AppError> {
        // Exits non-zero when no server runs; that's fine.
        self.run(&["kill-server"], Duration::from_secs(10))
            .await
            .map(|_| ())
    }

    pub async fn version(&self) -> Result<String, AppError> {
        let out = self.run_ok(&["version"], Duration::from_secs(10)).await?;
        Ok(out.lines().take(2).collect::<Vec<_>>().join(" / "))
    }

    pub async fn devices(&self) -> Result<Vec<Device>, AppError> {
        Ok(parse_devices(
            &self
                .run_ok(&["devices", "-l"], Duration::from_secs(15))
                .await?,
        ))
    }

    pub async fn usb_devices(&self) -> Result<Vec<Device>, AppError> {
        Ok(self
            .devices()
            .await?
            .into_iter()
            .filter(|d| d.transport == Transport::Usb)
            .collect())
    }

    /// `adb connect` exits 0 even when it fails, so interpret its text.
    pub async fn connect(&self, serial: &str) -> Result<(), AppError> {
        let out = self
            .run(&["connect", serial], Duration::from_secs(20))
            .await?;
        let text = out.text();
        let lower = text.to_lowercase();
        if lower.contains("connected to") && !lower.contains("failed") && !lower.contains("cannot")
        {
            Ok(())
        } else {
            Err(AppError::new(
                ErrorCode::Adb,
                format!("adb connect {serial}: {text}"),
            ))
        }
    }

    pub async fn disconnect(&self, serial: &str) -> Result<(), AppError> {
        self.run(&["disconnect", serial], Duration::from_secs(10))
            .await
            .map(|_| ())
    }

    /// State of one device, or `None` if adb doesn't know it.
    pub async fn get_state(&self, serial: &str) -> Result<Option<DeviceState>, AppError> {
        let devices = self.devices().await?;
        Ok(devices
            .into_iter()
            .find(|d| d.serial == serial)
            .map(|d| d.state))
    }

    pub async fn shell(
        &self,
        serial: &str,
        command: &str,
        timeout: Duration,
    ) -> Result<String, AppError> {
        self.run_ok(&["-s", serial, "shell", command], timeout)
            .await
    }

    /// Restart adbd on the phone listening on TCP `port` (over USB).
    pub async fn tcpip(&self, usb_serial: &str, port: u16) -> Result<(), AppError> {
        let p = port.to_string();
        let out = self
            .run(&["-s", usb_serial, "tcpip", &p], Duration::from_secs(20))
            .await?;
        let text = out.text();
        if out.success && !text.to_lowercase().contains("error") {
            Ok(())
        } else {
            Err(AppError::new(
                ErrorCode::Adb,
                format!("adb tcpip {port}: {text}"),
            ))
        }
    }

    /// The phone's Tailscale IPv4 (from its tun interface), read over USB.
    pub async fn tailnet_ipv4(&self, serial: &str) -> Result<Option<Ipv4Addr>, AppError> {
        let out = self
            .shell(serial, "ip -4 -o addr show", Duration::from_secs(10))
            .await?;
        Ok(parse_tailnet_ipv4(&out))
    }

    pub async fn getprop(&self, serial: &str, prop: &str) -> Result<String, AppError> {
        Ok(self
            .shell(serial, &format!("getprop {prop}"), Duration::from_secs(10))
            .await?
            .trim()
            .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_devices_l() {
        let text = "List of devices attached\n\
            * daemon not running; starting now at tcp:5037\n\
            * daemon started successfully\n\
            1A2B3C4D       device usb:1-1 product:tokay model:Pixel_9 device:tokay transport_id:1\n\
            127.0.0.1:41234 device product:tokay model:Pixel_9 device:tokay transport_id:2\n\
            ZX1         unauthorized usb:1-2 transport_id:3\n\
            ABC  no permissions (missing udev rules); see [http://developer.android.com/tools/device.html] usb:1-3 transport_id:4\n\
            emulator-5554  offline transport_id:5\n";
        let d = parse_devices(text);
        assert_eq!(d.len(), 5);
        assert_eq!(d[0].serial, "1A2B3C4D");
        assert_eq!(d[0].state, DeviceState::Device);
        assert_eq!(d[0].transport, Transport::Usb);
        assert_eq!(d[0].model.as_deref(), Some("Pixel_9"));
        assert_eq!(d[1].transport, Transport::Tcp);
        assert_eq!(d[2].state, DeviceState::Unauthorized);
        assert_eq!(d[3].state, DeviceState::NoPermissions);
        assert_eq!(d[4].transport, Transport::Tcp);
        assert_eq!(d[4].state, DeviceState::Offline);
    }

    #[test]
    fn finds_tailnet_ip() {
        let text = "1: lo    inet 127.0.0.1/8 scope host lo\n\
            30: wlan0    inet 192.168.1.20/24 brd 192.168.1.255 scope global wlan0\n\
            41: rmnet_data2    inet 10.161.4.5/30 scope global rmnet_data2\n\
            52: tun0    inet 100.101.102.103/32 scope global tun0\n";
        assert_eq!(
            parse_tailnet_ipv4(text),
            Some(Ipv4Addr::new(100, 101, 102, 103))
        );
        // 100.0.0.0/8 outside the CGNAT range is not a tailnet address.
        assert_eq!(parse_tailnet_ipv4("inet 100.200.1.1/32"), None);
        assert_eq!(
            parse_tailnet_ipv4("inet 100.127.255.1/32"),
            Some(Ipv4Addr::new(100, 127, 255, 1))
        );
    }
}
