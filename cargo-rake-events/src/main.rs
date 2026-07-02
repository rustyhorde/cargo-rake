//! A dev-only listener that prints lifecycle events sent by a licensed
//! `rake`/`cargo rake` run to the loopback UDP address configured in a
//! Rakefile's `[lifecycle]` table. Never published; not part of the
//! rake/cargo-rake CLI surface.

use std::error::Error;
use std::net::SocketAddr;

use tokio::net::UdpSocket;

const LISTEN_ADDR: &str = "127.0.0.1:9999";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let socket = UdpSocket::bind(LISTEN_ADDR).await?;
    println!("listening for lifecycle events on {LISTEN_ADDR}");

    let mut buf = [0_u8; 4096];
    loop {
        let (len, addr) = socket.recv_from(&mut buf).await?;
        println!("{}", format_event(&buf[..len], addr));
    }
}

/// Renders one received datagram as a labeled, human-readable block: pretty
/// JSON when the bytes parse as JSON, otherwise the raw bytes as best-effort
/// UTF-8 (this listener has no control over what hits the socket).
fn format_event(bytes: &[u8], addr: SocketAddr) -> String {
    match serde_json::from_slice::<serde_json::Value>(bytes) {
        Ok(value) => match serde_json::to_string_pretty(&value) {
            Ok(pretty) => format!("--- from {addr} ---\n{pretty}"),
            Err(_) => format!(
                "--- from {addr} (raw) ---\n{}",
                String::from_utf8_lossy(bytes)
            ),
        },
        Err(_) => format!(
            "--- from {addr} (non-json) ---\n{}",
            String::from_utf8_lossy(bytes)
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::net::SocketAddr;

    use super::format_event;

    fn addr() -> Result<SocketAddr, Box<dyn Error>> {
        Ok("127.0.0.1:9999".parse()?)
    }

    #[test]
    fn formats_valid_json_as_pretty_block() -> Result<(), Box<dyn Error>> {
        let rendered = format_event(br#"{"event":"before_all","target_count":1}"#, addr()?);
        assert!(rendered.starts_with("--- from 127.0.0.1:9999 ---\n"));
        assert!(rendered.contains("\"event\": \"before_all\""));
        assert!(rendered.contains("\"target_count\": 1"));
        Ok(())
    }

    #[test]
    fn formats_non_json_bytes_as_raw_fallback() -> Result<(), Box<dyn Error>> {
        let rendered = format_event(b"not json", addr()?);
        assert_eq!(rendered, "--- from 127.0.0.1:9999 (non-json) ---\nnot json");
        Ok(())
    }
}
