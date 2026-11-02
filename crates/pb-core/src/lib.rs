//! PhoneBridge core. No Tauri dependency, so it can be tested on its own.
//!
//! - [`tailnet`]: the app's own userspace Tailscale node (libtailscale/tsnet)
//! - [`forwarder`]: `127.0.0.1:<port>` → tailnet → phone:5555, for adb
//! - [`adb`], [`scrcpy`]: run the bundled binaries with an isolated environment
//! - [`connection`]: keeps the saved phone connected and explains its state
//! - [`procs`]: scrcpy window processes
//! - [`setup`]: the "Set up via USB" steps

pub mod adb;
pub mod connection;
pub mod error;
pub mod forwarder;
pub mod presets;
pub mod procs;
pub mod scrcpy;
pub mod setup;
pub mod tailnet;
pub mod tools;

pub use error::{AppError, ErrorCode};

/// Tailnet hostname of the app's node.
pub const DEFAULT_HOSTNAME: &str = "phonebridge-mac";
