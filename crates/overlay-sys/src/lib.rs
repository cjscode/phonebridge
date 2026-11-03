//! Raw bindings to `go/` (built into `liboverlay.a`): `nb_*` drive an embedded
//! NetBird client (`go/netbird.go`), `ts_*` an embedded Tailscale node
//! (`go/tailscale.go`).
//!
//! Conventions: functions return 0 on success and -1 on error; on error
//! `*err_out` (if non-null) gets a malloc'd message. Every string handed back
//! through an out-parameter must be released with [`ov_free`]. Connections are
//! socketpair file descriptors (close-on-exec).
//!
//! Use the safe wrapper in `pb-core::overlay` instead of calling these directly.

use std::os::raw::{c_char, c_int};

unsafe extern "C" {
    pub fn ov_free(p: *mut c_char);

    // ---- NetBird ----

    /// `opts_json`: {device_name, management_url, config_path, state_path,
    /// setup_key | jwt_token | use_saved_key, log_fd, log_level}. Returns a handle.
    pub fn nb_new(opts_json: *const c_char, err_out: *mut *mut c_char) -> c_int;
    /// Log in and bring the engine up. Blocks up to `timeout_ms`.
    pub fn nb_start(h: c_int, timeout_ms: c_int, err_out: *mut *mut c_char) -> c_int;
    /// Stop and release the handle.
    pub fn nb_stop(h: c_int, err_out: *mut *mut c_char) -> c_int;
    pub fn nb_status_json(h: c_int, out: *mut *mut c_char, err_out: *mut *mut c_char) -> c_int;
    pub fn nb_dial(
        h: c_int,
        network: *const c_char,
        addr: *const c_char,
        timeout_ms: c_int,
        fd_out: *mut c_int,
        err_out: *mut *mut c_char,
    ) -> c_int;

    /// Start a browser login. `out` gets JSON {id, kind, url, user_code, expires_in}.
    pub fn nb_sso_begin(
        mgmt_url: *const c_char,
        out: *mut *mut c_char,
        err_out: *mut *mut c_char,
    ) -> c_int;
    /// Block until the login finishes; `token_out` gets the token for `jwt_token`.
    pub fn nb_sso_wait(id: c_int, token_out: *mut *mut c_char, err_out: *mut *mut c_char) -> c_int;
    pub fn nb_sso_cancel(id: c_int);

    /// Tell the management server this peer logged out (uses the key in `config_path`).
    pub fn nb_logout(
        mgmt_url: *const c_char,
        config_path: *const c_char,
        err_out: *mut *mut c_char,
    ) -> c_int;

    // ---- Tailscale ----

    /// `opts_json`: {hostname, state_dir, control_url, auth_key, log_fd}. Returns a handle.
    pub fn ts_new(opts_json: *const c_char, err_out: *mut *mut c_char) -> c_int;
    /// Start, then wait up to `timeout_ms` until running / needs login / error.
    pub fn ts_start(h: c_int, timeout_ms: c_int, err_out: *mut *mut c_char) -> c_int;
    /// Stop and release the handle.
    pub fn ts_stop(h: c_int, err_out: *mut *mut c_char) -> c_int;
    /// Same JSON shape as `nb_status_json`, plus backend_state/auth_url/health.
    pub fn ts_status_json(h: c_int, out: *mut *mut c_char, err_out: *mut *mut c_char) -> c_int;
    /// Ask for a new login URL (appears as `auth_url` in the status).
    pub fn ts_login(h: c_int, err_out: *mut *mut c_char) -> c_int;
    /// Log out at the control server.
    pub fn ts_logout(h: c_int, err_out: *mut *mut c_char) -> c_int;
    pub fn ts_dial(
        h: c_int,
        network: *const c_char,
        addr: *const c_char,
        timeout_ms: c_int,
        fd_out: *mut c_int,
        err_out: *mut *mut c_char,
    ) -> c_int;
}
