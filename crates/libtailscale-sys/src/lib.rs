//! Raw bindings to `tailscale.h` from tailscale/libtailscale (see `vendor/libtailscale`).
//!
//! All handles are plain C ints. Connections and listeners are file descriptors:
//! a connection is one end of a socketpair that can be read/written/closed
//! like any socket. Handed-out fds are close-on-exec (libtailscale ac7ed43+), so
//! they don't leak into child processes.
//!
//! Use the safe wrapper in `pb-core::tailnet` rather than calling these directly.

#![allow(non_camel_case_types)]

use std::os::raw::{c_char, c_int};

/// Handle to a tsnet server.
pub type tailscale = c_int;
/// A connection fd (socketpair end).
pub type tailscale_conn = c_int;
/// A listener fd.
pub type tailscale_listener = c_int;

unsafe extern "C" {
    pub fn tailscale_new() -> tailscale;
    pub fn tailscale_start(sd: tailscale) -> c_int;
    pub fn tailscale_up(sd: tailscale) -> c_int;
    pub fn tailscale_close(sd: tailscale) -> c_int;

    pub fn tailscale_set_dir(sd: tailscale, dir: *const c_char) -> c_int;
    pub fn tailscale_set_hostname(sd: tailscale, hostname: *const c_char) -> c_int;
    pub fn tailscale_set_authkey(sd: tailscale, authkey: *const c_char) -> c_int;
    pub fn tailscale_set_control_url(sd: tailscale, control_url: *const c_char) -> c_int;
    pub fn tailscale_set_ephemeral(sd: tailscale, ephemeral: c_int) -> c_int;
    pub fn tailscale_set_logfd(sd: tailscale, fd: c_int) -> c_int;

    pub fn tailscale_getips(sd: tailscale, buf: *mut c_char, buflen: usize) -> c_int;

    pub fn tailscale_dial(
        sd: tailscale,
        network: *const c_char,
        addr: *const c_char,
        conn_out: *mut tailscale_conn,
    ) -> c_int;

    pub fn tailscale_listen(
        sd: tailscale,
        network: *const c_char,
        addr: *const c_char,
        listener_out: *mut tailscale_listener,
    ) -> c_int;

    pub fn tailscale_getremoteaddr(
        l: tailscale_listener,
        conn: tailscale_conn,
        buf: *mut c_char,
        buflen: usize,
    ) -> c_int;

    pub fn tailscale_accept(listener: tailscale_listener, conn_out: *mut tailscale_conn) -> c_int;

    /// `addr_out` gets "ip:port"; both credential buffers must hold 33 bytes.
    pub fn tailscale_loopback(
        sd: tailscale,
        addr_out: *mut c_char,
        addrlen: usize,
        proxy_cred_out: *mut c_char,
        local_api_cred_out: *mut c_char,
    ) -> c_int;

    /// Writes a malloc'd, NUL-terminated ipnstate.Status JSON to `*json_out`; free with `libc::free`.
    pub fn tailscale_status_json(sd: tailscale, json_out: *mut *mut c_char) -> c_int;

    pub fn tailscale_errmsg(sd: tailscale, buf: *mut c_char, buflen: usize) -> c_int;
}
