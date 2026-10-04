//! wol-rs: 零依賴 Wake-on-LAN 工具
//! - CLI:  `wol <mac>`              直接在 LAN 播送 magic packet
//! - HTTP: `wol` 啟動伺服器(預設 :8787),GET/POST /wake?mac=AA:BB:CC:DD:EE:FF
//!
//! 用途:RPi3 常開 + cloudflared tunnel,異地喚醒有線 PC。

use std::env;
use std::io::{Read, Write};
use std::net::{SocketAddr, SocketAddrV4, UdpSocket};
use std::thread;

const MAGIC_PORT: u16 = 9;

fn send_wol(mac: [u8; 6]) -> std::io::Result<SocketAddr> {
    // 6 個 0xFF + 重複 16 次 MAC
    let packet = [vec![0xFFu8; 6], mac.to_vec().repeat(16)].concat();

    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_broadcast(true)?;
    // 播送:LAN 內任何位置的網卡都能收到;若 PC 有固定 IP 也可改成單播
    let to = SocketAddr::V4(SocketAddrV4::new([255, 255, 255, 255].into(), MAGIC_PORT));
    sock.send_to(&packet, to)?;
    Ok(to)
}

fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 6 {
        return None;
    }
    let mut out = [0u8; 6];
    for (i, p) in parts.iter().enumerate() {
        out[i] = u8::from_str_radix(p, 16).ok()?;
    }
    Some(out)
}

fn handle(client: &mut std::net::TcpStream) {
    let mut buf = [0u8; 4096];
    let n = client.read(&mut buf).unwrap_or(0);
    let req = String::from_utf8_lossy(&buf[..n]);
    let first_line = req.lines().next().unwrap_or("");
    let mut it = first_line.split_whitespace();
    let method = it.next().unwrap_or("");
    let path = it.next().unwrap_or("/");

    let (status, body): (&str, String) = if method == "GET" || method == "POST" {
        // GET/POST /wake?mac=AA:BB...[:cc]
        let mac_str = path
            .split('?')
            .nth(1)
            .and_then(|q| {
                q.split('&')
                    .find_map(|kv| kv.strip_prefix("mac=").map(|s| s.to_lowercase()))
            })
            .unwrap_or_default();
        match parse_mac(&mac_str) {
            Some(mac) => match send_wol(mac) {
                Ok(_) => ("200 OK", format!("woken: {}", mac_str)),
                Err(e) => ("500 Internal Server Error", format!("send failed: {}", e)),
            },
            None => ("400 Bad Request", "usage: /wake?mac=AA:BB:CC:DD:EE:FF".into()),
        }
    } else {
        ("405 Method Not Allowed", "use GET or POST".into())
    };

    let resp = format!(
        "HTTP/1.1 {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n{}",
        status,
        body.len(),
        body
    );
    let _ = client.write_all(resp.as_bytes());
}

fn main() {
    let mut args = env::args().skip(1);
    let first = args.next();

    // CLI 模式: wol aa:bb:cc:dd:ee:ff  →  直接播送一次
    if let Some(mac) = first.and_then(|m| parse_mac(m.as_str())) {
        send_wol(mac).expect("wol send");
        return;
    }

    // 伺服器模式,預設 port 8787,可用 PORT env 改
    let port: u16 = env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8787);
    let listener = std::net::TcpListener::bind(("0.0.0.0", port)).expect("bind 0.0.0.0:port");
    println!("wol server on 0.0.0.0:{}", port);
    for stream in listener.incoming() {
        if let Ok(mut s) = stream {
            thread::spawn(move || handle(&mut s));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_mac;

    #[test]
    fn valid_mac() {
        assert_eq!(parse_mac("aa:bb:cc:dd:ee:ff"), Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]));
        assert_eq!(parse_mac("AA:BB:CC:DD:EE:FF"), Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]));
    }

    #[test]
    fn invalid_mac() {
        assert_eq!(parse_mac("aabbccddeeff"), None);
        assert_eq!(parse_mac("aa:bb:cc:dd:ee"), None);
        assert_eq!(parse_mac("zz:bb:cc:dd:ee:ff"), None);
    }
}
