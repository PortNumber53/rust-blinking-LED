//! A tiny HTTP/1.1 server for the captive portal.
//!
//! It serves these over TCP port 80:
//!   - `GET /`            → the portal page (network list + connect form).
//!   - `GET /networks`    → JSON array of scanned networks (the page fetches it).
//!   - `POST /connect`    → body `ssid=...&password=...`; raises a connect request.
//!   - `GET /status`      → JSON `{"status":"..."}` polled by the page.
//!   - everything else    → a 302 redirect to `/`, which is what makes OS captive
//!     -portal probes (e.g. `/generate_204`, `/hotspot-detect.html`) trigger the
//!     login UI.
//!
//! The server is intentionally minimal: it reads one request, writes one
//! response, and closes. No keep-alive, no chunked encoding.

use embassy_net::tcp::TcpSocket;
use embassy_net::Stack;
use embassy_time::Duration;
use embedded_io_async::Write;
use heapless::String;

use crate::dhcp_server::SERVER_IP;
use crate::shared::{self, ConnectRequest, PASSWORD_MAX};
use crate::wifi::SSID_MAX;

/// Embedded portal HTML (served at `/`).
const PORTAL_HTML: &str = include_str!("portal.html");

/// Per-connection buffers. One request/response cycle uses these.
const RX_LEN: usize = 2048;
const TX_LEN: usize = 4096;

/// The HTTP server task. Runs for the whole program; after the STA switch it
/// keeps serving the status page (now reachable on the upstream-assigned IP).
#[embassy_executor::task]
pub async fn http_server_task(stack: Stack<'static>) -> ! {
    let mut rx_buffer = [0u8; RX_LEN];
    let mut tx_buffer = [0u8; TX_LEN];
    let mut req_buf = [0u8; RX_LEN];

    loop {
        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(Duration::from_secs(10)));

        if socket.accept(80u16).await.is_err() {
            continue;
        }

        // Read the request (headers + small body) into req_buf.
        let n = match read_request(&mut socket, &mut req_buf).await {
            Some(n) => n,
            None => {
                socket.close();
                continue;
            }
        };

        handle_request(&mut socket, &req_buf[..n]).await;
        socket.close();
        // Give the stack a moment to flush the FIN before reusing buffers.
        embassy_futures::yield_now().await;
    }
}

/// Read until we have the full headers (and any small body that fits the buffer).
/// Returns the number of bytes read, or None on error/empty.
async fn read_request(socket: &mut TcpSocket<'_>, buf: &mut [u8]) -> Option<usize> {
    let mut total = 0;
    loop {
        if total >= buf.len() {
            break; // buffer full; treat what we have as the request
        }
        match socket.read(&mut buf[total..]).await {
            Ok(0) => break, // peer closed
            Ok(n) => total += n,
            Err(_) => return None,
        }
        // Stop once we've seen the end of the header block. For POSTs with a
        // body, the body usually arrives in the same segment for our tiny forms.
        if let Some(headers_end) = find_subslice(&buf[..total], b"\r\n\r\n") {
            if buf[..total].starts_with(b"POST") {
                if let Some(len) = content_length(&buf[..headers_end]) {
                    let body_have = total - (headers_end + 4);
                    if body_have >= len {
                        break;
                    }
                    // else keep reading for the rest of the body
                } else {
                    break;
                }
            } else {
                break;
            }
        }
    }
    if total == 0 {
        None
    } else {
        Some(total)
    }
}

/// Route the request and write the response.
async fn handle_request(socket: &mut TcpSocket<'_>, req: &[u8]) {
    let (method, path) = parse_request_line(req).unwrap_or(("GET", "/"));

    match (method, path) {
        ("GET", "/") => {
            send_response(socket, "200 OK", "text/html", PORTAL_HTML.as_bytes()).await;
        }
        ("GET", "/networks") => {
            send_networks(socket).await;
        }
        ("GET", "/status") => {
            send_status(socket).await;
        }
        ("POST", "/connect") => {
            handle_connect(socket, req).await;
        }
        // Captive-portal probes and anything else → redirect to the portal.
        _ => {
            send_redirect(socket).await;
        }
    }
}

/// Build and send the network list as JSON: `[{"ssid":"..","rssi":-40},..]`.
async fn send_networks(socket: &mut TcpSocket<'_>) {
    let mut body: String<2048> = String::new();
    let _ = body.push('[');
    {
        let list = shared::NETWORKS.lock().await;
        for (i, n) in list.iter().enumerate() {
            if i > 0 {
                let _ = body.push(',');
            }
            let _ = body.push_str("{\"ssid\":\"");
            push_json_escaped(&mut body, n.ssid.as_str());
            let _ = body.push_str("\",\"rssi\":");
            push_i16(&mut body, n.rssi);
            let _ = body.push('}');
        }
    }
    let _ = body.push(']');
    send_response(socket, "200 OK", "application/json", body.as_bytes()).await;
}

/// Send the current status as JSON: `{"status":"portal"}`.
async fn send_status(socket: &mut TcpSocket<'_>) {
    let s = shared::status().await;
    let mut body: String<64> = String::new();
    let _ = body.push_str("{\"status\":\"");
    let _ = body.push_str(s.as_str());
    let _ = body.push_str("\"}");
    send_response(socket, "200 OK", "application/json", body.as_bytes()).await;
}

/// Parse `ssid=...&password=...` from the POST body and raise a connect request.
async fn handle_connect(socket: &mut TcpSocket<'_>, req: &[u8]) {
    let body = match find_subslice(req, b"\r\n\r\n") {
        Some(idx) => &req[idx + 4..],
        None => b"",
    };

    let mut ssid: String<SSID_MAX> = String::new();
    let mut password: String<PASSWORD_MAX> = String::new();
    parse_form(body, &mut ssid, &mut password);

    if ssid.is_empty() {
        send_response(socket, "400 Bad Request", "text/plain", b"missing ssid").await;
        return;
    }

    // Hand the request to the main state machine and report "connecting".
    shared::set_status(shared::Status::Connecting).await;
    shared::CONNECT.signal(ConnectRequest { ssid, password });

    send_response(socket, "200 OK", "application/json", b"{\"ok\":true}").await;
}

/// Send a 302 redirect to the portal root at the AP IP. Used for captive probes.
async fn send_redirect(socket: &mut TcpSocket<'_>) {
    let mut location: String<64> = String::new();
    let _ = location.push_str("http://");
    push_ip(&mut location, &SERVER_IP.octets());
    let _ = location.push('/');

    let mut head: String<256> = String::new();
    let _ = head.push_str("HTTP/1.1 302 Found\r\nLocation: ");
    let _ = head.push_str(location.as_str());
    let _ = head.push_str("\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let _ = socket.write_all(head.as_bytes()).await;
    let _ = socket.flush().await;
}

/// Write a full HTTP response with the given status, content type, and body.
async fn send_response(socket: &mut TcpSocket<'_>, status: &str, content_type: &str, body: &[u8]) {
    let mut head: String<256> = String::new();
    let _ = head.push_str("HTTP/1.1 ");
    let _ = head.push_str(status);
    let _ = head.push_str("\r\nContent-Type: ");
    let _ = head.push_str(content_type);
    let _ = head.push_str("\r\nContent-Length: ");
    push_usize(&mut head, body.len());
    let _ = head.push_str("\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n");

    if socket.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    let _ = socket.write_all(body).await;
    let _ = socket.flush().await;
}

// ---- small parsing / formatting helpers (no_std, no alloc) ----

/// Parse the request line, returning (method, path).
fn parse_request_line(req: &[u8]) -> Option<(&str, &str)> {
    let line_end = find_subslice(req, b"\r\n").unwrap_or(req.len());
    let line = core::str::from_utf8(&req[..line_end]).ok()?;
    let mut parts = line.split(' ');
    let method = parts.next()?;
    let path = parts.next()?;
    Some((method, path))
}

/// Extract Content-Length from a header block, if present.
fn content_length(headers: &[u8]) -> Option<usize> {
    let text = core::str::from_utf8(headers).ok()?;
    for line in text.split("\r\n") {
        if let Some(rest) = line
            .strip_prefix("Content-Length:")
            .or_else(|| line.strip_prefix("content-length:"))
        {
            return rest.trim().parse::<usize>().ok();
        }
    }
    None
}

/// Parse `ssid=...&password=...` (URL-encoded) into the provided buffers.
fn parse_form<const A: usize, const B: usize>(
    body: &[u8],
    ssid: &mut String<A>,
    password: &mut String<B>,
) {
    let text = match core::str::from_utf8(body) {
        Ok(t) => t,
        Err(_) => return,
    };
    for pair in text.split('&') {
        let mut kv = pair.splitn(2, '=');
        let key = kv.next().unwrap_or("");
        let val = kv.next().unwrap_or("");
        match key {
            "ssid" => url_decode_into(val, ssid),
            "password" => url_decode_into(val, password),
            _ => {}
        }
    }
}

/// URL-decode `s` (handling `%XX` and `+`) into `out`, truncating on overflow.
fn url_decode_into<const N: usize>(s: &str, out: &mut String<N>) {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut tmp = [0u8; 4];
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'%' && i + 2 < bytes.len() {
            let hi = hex_val(bytes[i + 1]);
            let lo = hex_val(bytes[i + 2]);
            if let (Some(h), Some(l)) = (hi, lo) {
                let byte = (h << 4) | l;
                if let Some(ch) = char::from_u32(byte as u32) {
                    let _ = out.push_str(ch.encode_utf8(&mut tmp));
                }
                i += 3;
                continue;
            }
        }
        if c == b'+' {
            let _ = out.push(' ');
        } else {
            let _ = out.push(c as char);
        }
        i += 1;
    }
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Find the first index of `needle` in `haystack`.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Append a JSON-string-escaped version of `s` to `out`.
fn push_json_escaped<const N: usize>(out: &mut String<N>, s: &str) {
    for ch in s.chars() {
        match ch {
            '"' => {
                let _ = out.push_str("\\\"");
            }
            '\\' => {
                let _ = out.push_str("\\\\");
            }
            c if (c as u32) < 0x20 => {
                // Drop control chars rather than emit invalid JSON.
            }
            c => {
                let _ = out.push(c);
            }
        }
    }
}

fn push_usize<const N: usize>(out: &mut String<N>, mut v: usize) {
    if v == 0 {
        let _ = out.push('0');
        return;
    }
    let mut digits = [0u8; 20];
    let mut i = digits.len();
    while v > 0 {
        i -= 1;
        digits[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    for &d in &digits[i..] {
        let _ = out.push(d as char);
    }
}

fn push_i16<const N: usize>(out: &mut String<N>, v: i16) {
    if v < 0 {
        let _ = out.push('-');
        push_usize(out, (-(v as i32)) as usize);
    } else {
        push_usize(out, v as usize);
    }
}

fn push_ip<const N: usize>(out: &mut String<N>, octets: &[u8; 4]) {
    for (i, o) in octets.iter().enumerate() {
        if i > 0 {
            let _ = out.push('.');
        }
        push_usize(out, *o as usize);
    }
}
