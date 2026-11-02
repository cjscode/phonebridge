//! Raw bindings to `go/shim.go` (built into `libnetbird.a`).
//!
//! Conventions: functions return 0 on success and -1 on error; on error
//! `*err_out` (if non-null) gets a malloc'd message. Every string handed back
//! through an out-parameter must be released with [`nb_free`]. Connections are
//! socketpair file descriptors (close-on-exec).
//!
//! Use the safe wrapper in `pb-core::overlay` instead of calling these directly.

use std::os::raw::{c_char, c_int};

unsafe extern "C" {
    pub fn nb_free(p: *mut c_char);

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
}
