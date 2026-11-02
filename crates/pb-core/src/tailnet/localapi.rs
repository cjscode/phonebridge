//! Minimal HTTP/1.1 client for the node's loopback LocalAPI.
//!
//! Only a couple of POSTs are needed (logout), so this avoids pulling in an HTTP
//! stack. Auth follows `tailscale.h`: header `Sec-Tailscale: localapi` and basic
//! auth `tsnet:<local_api_cred>`.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use base64::Engine;

use super::node::Loopback;
use crate::error::{AppError, ErrorCode};

pub fn post(lb: &Loopback, path: &str) -> Result<(u16, String), AppError> {
    let auth = base64::engine::general_purpose::STANDARD.encode(format!("tsnet:{}", lb.local_api_cred));
    let req = format!(
        "POST /localapi/v0/{path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Basic {auth}\r\n\
         Sec-Tailscale: localapi\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        host = lb.addr
    );
    let err = |e: std::io::Error| AppError::new(ErrorCode::Tailnet, format!("LocalAPI {path}: {e}"));
    let mut stream = TcpStream::connect(&lb.addr).map_err(err)?;
    stream.set_read_timeout(Some(Duration::from_secs(20))).map_err(err)?;
    stream.set_write_timeout(Some(Duration::from_secs(5))).map_err(err)?;
    stream.write_all(req.as_bytes()).map_err(err)?;
    let mut resp = Vec::new();
    stream.read_to_end(&mut resp).map_err(err)?;
    parse_response(&resp)
        .ok_or_else(|| AppError::new(ErrorCode::Tailnet, format!("LocalAPI {path}: malformed response")))
}

/// Returns (status code, body).
fn parse_response(resp: &[u8]) -> Option<(u16, String)> {
    let text = String::from_utf8_lossy(resp);
    let status = text.lines().next()?.split_whitespace().nth(1)?.parse().ok()?;
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
    Some((status, body))
}

/// Log this node out of the tailnet.
pub fn logout(lb: &Loopback) -> Result<(), AppError> {
    let (code, body) = post(lb, "logout")?;
    if (200..300).contains(&code) {
        Ok(())
    } else {
        Err(AppError::new(ErrorCode::Tailnet, format!("logout failed (HTTP {code}): {}", body.trim())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status_and_body() {
        let r = b"HTTP/1.1 204 No Content\r\nDate: x\r\n\r\n";
        assert_eq!(parse_response(r), Some((204, String::new())));
        let r = b"HTTP/1.1 403 Forbidden\r\nContent-Length: 5\r\n\r\nnope\n";
        assert_eq!(parse_response(r), Some((403, "nope\n".into())));
        assert_eq!(parse_response(b"garbage"), None);
    }

    #[test]
    fn sends_expected_headers() {
        use std::net::TcpListener;
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let server = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut buf = [0u8; 2048];
            let n = s.read(&mut buf).unwrap();
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });
        let lb = Loopback { addr, proxy_cred: "p".into(), local_api_cred: "secret".into() };
        logout(&lb).unwrap();
        let req = server.join().unwrap();
        assert!(req.starts_with("POST /localapi/v0/logout HTTP/1.1\r\n"));
        assert!(req.contains("Sec-Tailscale: localapi\r\n"));
        // base64("tsnet:secret")
        assert!(req.contains("Authorization: Basic dHNuZXQ6c2VjcmV0\r\n"));
    }
}
