//! A catch-all DNS server for the captive portal.
//!
//! While in AP mode every client is told (via DHCP option 6) that the Pico is
//! its DNS server. This task answers *every* A-record query with the AP's own
//! address (192.168.4.1). The effect: whatever hostname the client's OS probes
//! to detect a captive portal (e.g. `connectivitycheck.gstatic.com`,
//! `captive.apple.com`) resolves to us, the OS notices it didn't get the
//! expected content, and pops the portal web view.
//!
//! We do not fully parse the query. A DNS message is:
//!   [12-byte header][question section][answer section]...
//! We copy the request verbatim, flip it into a response (QR=1), set ANCOUNT=1,
//! and append a single compressed A answer pointing at the AP IP. The question
//! name is left untouched (answer uses a 0xC00C pointer back to it).

use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpListenEndpoint, Stack};
use embassy_time::{Duration, Timer};

use crate::dhcp_server::SERVER_IP;

const DNS_PORT: u16 = 53;
/// DNS header is 12 bytes: ID(2) FLAGS(2) QDCOUNT(2) ANCOUNT(2) NSCOUNT(2) ARCOUNT(2).
const DNS_HEADER_LEN: usize = 12;

/// Build a catch-all response into `out` from request `req` (length `req_len`).
/// Returns the response length, or 0 if the request is not answerable.
fn build_response(req: &[u8], req_len: usize, out: &mut [u8]) -> usize {
    if req_len < DNS_HEADER_LEN {
        return 0;
    }
    // QDCOUNT must be at least 1 for us to have a question to point back at.
    let qdcount = u16::from_be_bytes([req[4], req[5]]);
    if qdcount == 0 {
        return 0;
    }

    // Copy the whole request (header + question section) as the basis.
    out[..req_len].copy_from_slice(&req[..req_len]);

    // FLAGS: set QR=1 (response), keep opcode/RD from the request, set RA=1,
    // RCODE=0. High byte: 1_0000_0_0_1 with RD echoed; low byte: RA + rcode.
    let rd = req[2] & 0x01; // recursion-desired bit from the request
    out[2] = 0x80 | rd; // QR=1, opcode=0, AA=0, TC=0, RD echoed
    out[3] = 0x80; // RA=1, Z=0, RCODE=0

    // ANCOUNT = 1.
    out[6] = 0x00;
    out[7] = 0x01;
    // NSCOUNT / ARCOUNT = 0 (in case the request set them).
    out[8] = 0;
    out[9] = 0;
    out[10] = 0;
    out[11] = 0;

    // Append the answer record after the copied question section.
    let mut pos = req_len;
    // NAME: pointer to the question name at offset 12 (0xC0 0x0C).
    out[pos] = 0xC0;
    out[pos + 1] = 0x0C;
    pos += 2;
    // TYPE = A (1).
    out[pos] = 0x00;
    out[pos + 1] = 0x01;
    pos += 2;
    // CLASS = IN (1).
    out[pos] = 0x00;
    out[pos + 1] = 0x01;
    pos += 2;
    // TTL = 60 seconds.
    out[pos..pos + 4].copy_from_slice(&60u32.to_be_bytes());
    pos += 4;
    // RDLENGTH = 4.
    out[pos] = 0x00;
    out[pos + 1] = 0x04;
    pos += 2;
    // RDATA = the AP IP.
    out[pos..pos + 4].copy_from_slice(&SERVER_IP.octets());
    pos += 4;

    pos
}

/// The DNS catch-all task. Like the DHCP task it runs for the whole program;
/// after the STA switch no clients query it, so it idles on recv.
#[embassy_executor::task]
pub async fn dns_server_task(stack: Stack<'static>) -> ! {
    let mut rx_meta = [PacketMetadata::EMPTY; 8];
    let mut rx_buffer = [0u8; 1024];
    let mut tx_meta = [PacketMetadata::EMPTY; 8];
    let mut tx_buffer = [0u8; 1024];
    let mut recv_buf = [0u8; 512];
    let mut send_buf = [0u8; 600];

    loop {
        let mut socket = UdpSocket::new(
            stack,
            &mut rx_meta,
            &mut rx_buffer,
            &mut tx_meta,
            &mut tx_buffer,
        );

        if socket
            .bind(IpListenEndpoint { addr: None, port: DNS_PORT })
            .is_err()
        {
            Timer::after(Duration::from_millis(200)).await;
            continue;
        }

        loop {
            let (n, meta) = match socket.recv_from(&mut recv_buf).await {
                Ok(v) => v,
                Err(_) => break,
            };
            let resp_len = build_response(&recv_buf, n, &mut send_buf);
            if resp_len == 0 {
                continue;
            }
            // Reply to whoever asked (meta.endpoint carries the client addr:port).
            let _ = socket.send_to(&send_buf[..resp_len], meta.endpoint).await;
        }
    }
}
