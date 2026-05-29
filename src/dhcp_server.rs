//! A minimal DHCPv4 server for the captive-portal access point.
//!
//! embassy-net only ships a DHCP *client*, so we hand-roll the server side.
//! It is deliberately tiny: it understands DISCOVER and REQUEST, replies with
//! OFFER and ACK, and hands out addresses from a small linear pool. There is no
//! persistent lease database — leases are tracked in a fixed-size table in RAM
//! and we never run out in practice (a captive portal has a handful of clients).
//!
//! Everything the client needs to treat the Pico as its gateway is advertised:
//! the Pico's own address is sent as router (option 3) and DNS server (option 6),
//! so all the client's DNS queries arrive at our catch-all resolver and every
//! name resolves to the portal.

use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpAddress, IpEndpoint, IpListenEndpoint, Ipv4Address, Stack};
use embassy_time::{Duration, Timer};

/// The AP's own IPv4 address. Acts as server-id, router, and DNS for clients.
pub const SERVER_IP: Ipv4Address = Ipv4Address::new(192, 168, 4, 1);
/// The four octets of [`SERVER_IP`], for embedding in DHCP options.
const SERVER_OCTETS: [u8; 4] = [192, 168, 4, 1];
/// 255.255.255.0 — the /24 the AP hands out.
const SUBNET_MASK: [u8; 4] = [255, 255, 255, 0];
/// First address of the lease pool: 192.168.4.10 .. 192.168.4.10 + POOL_SIZE.
const POOL_BASE_LAST_OCTET: u8 = 10;
/// How many distinct clients we can lease to at once.
const POOL_SIZE: usize = 32;
/// Lease time advertised to clients (seconds). Short, since this AP is transient.
const LEASE_SECS: u32 = 3600;

// BOOTP / DHCP wire-format constants.
const DHCP_SERVER_PORT: u16 = 67;
const DHCP_CLIENT_PORT: u16 = 68;
const OP_BOOTREQUEST: u8 = 1;
const OP_BOOTREPLY: u8 = 2;
const HTYPE_ETHERNET: u8 = 1;
const HLEN_ETHERNET: u8 = 6;
const MAGIC_COOKIE: [u8; 4] = [99, 130, 83, 99];

// DHCP option codes.
const OPT_PAD: u8 = 0;
const OPT_SUBNET_MASK: u8 = 1;
const OPT_ROUTER: u8 = 3;
const OPT_DNS: u8 = 6;
const OPT_LEASE_TIME: u8 = 51;
const OPT_MESSAGE_TYPE: u8 = 53;
const OPT_SERVER_ID: u8 = 54;
const OPT_END: u8 = 255;

// DHCP message types (option 53 values).
const DHCPDISCOVER: u8 = 1;
const DHCPOFFER: u8 = 2;
const DHCPREQUEST: u8 = 3;
const DHCPACK: u8 = 5;
const DHCPNAK: u8 = 6;

/// One entry in our in-RAM lease table: which MAC holds which last-octet.
#[derive(Clone, Copy)]
struct Lease {
    mac: [u8; 6],
    last_octet: u8,
    used: bool,
}

impl Lease {
    const EMPTY: Lease = Lease {
        mac: [0; 6],
        last_octet: 0,
        used: false,
    };
}

/// Pick (or reuse) an address for a client identified by `mac`.
/// Returns the last octet of the leased 192.168.4.x address.
fn allocate(table: &mut [Lease; POOL_SIZE], mac: &[u8; 6]) -> Option<u8> {
    // Reuse an existing lease for this MAC if present (sticky addresses).
    for entry in table.iter() {
        if entry.used && entry.mac == *mac {
            return Some(entry.last_octet);
        }
    }
    // Otherwise take the first free slot.
    for (i, entry) in table.iter_mut().enumerate() {
        if !entry.used {
            let last_octet = POOL_BASE_LAST_OCTET + i as u8;
            entry.mac = *mac;
            entry.last_octet = last_octet;
            entry.used = true;
            return Some(last_octet);
        }
    }
    None // pool exhausted
}

/// Read DHCP option 53 (message type) from the options area of a packet.
fn message_type(options: &[u8]) -> Option<u8> {
    find_option(options, OPT_MESSAGE_TYPE).and_then(|v| v.first().copied())
}

/// Walk the TLV option list and return the value slice for `code`, if present.
fn find_option(options: &[u8], code: u8) -> Option<&[u8]> {
    let mut i = 0;
    while i < options.len() {
        let opt = options[i];
        if opt == OPT_END {
            break;
        }
        if opt == OPT_PAD {
            i += 1;
            continue;
        }
        // Every non-PAD/END option is [code][len][value...].
        if i + 1 >= options.len() {
            break;
        }
        let len = options[i + 1] as usize;
        let start = i + 2;
        let end = start + len;
        if end > options.len() {
            break;
        }
        if opt == code {
            return Some(&options[start..end]);
        }
        i = end;
    }
    None
}

/// Append a TLV option to `buf` at `*pos`, advancing `*pos`.
fn put_option(buf: &mut [u8], pos: &mut usize, code: u8, value: &[u8]) {
    buf[*pos] = code;
    buf[*pos + 1] = value.len() as u8;
    buf[*pos + 2..*pos + 2 + value.len()].copy_from_slice(value);
    *pos += 2 + value.len();
}

/// Build a BOOTREPLY into `out` from the incoming `req`, offering/acking
/// `offered_ip`. Returns the number of bytes written.
fn build_reply(req: &[u8], out: &mut [u8], offered_ip: [u8; 4], msg_type: u8) -> usize {
    // Zero the fixed BOOTP header region we touch.
    out[..240].fill(0);

    out[0] = OP_BOOTREPLY;
    out[1] = HTYPE_ETHERNET;
    out[2] = HLEN_ETHERNET;
    out[3] = 0; // hops
                // xid (4 bytes, offset 4): echo from request.
    out[4..8].copy_from_slice(&req[4..8]);
    // secs (8..10) and flags (10..12): echo from request (keep broadcast flag).
    out[10..12].copy_from_slice(&req[10..12]);
    // ciaddr 12..16 = 0. yiaddr 16..20 = the address we hand out.
    out[16..20].copy_from_slice(&offered_ip);
    // siaddr 20..24 = next server = us.
    out[20..24].copy_from_slice(&SERVER_OCTETS);
    // giaddr 24..28 = 0 (no relay).
    // chaddr 28..44: echo client hardware address from request.
    out[28..44].copy_from_slice(&req[28..44]);
    // sname (64) + file (128) left zero. Magic cookie at 236..240.
    out[236..240].copy_from_slice(&MAGIC_COOKIE);

    let mut pos = 240;
    put_option(out, &mut pos, OPT_MESSAGE_TYPE, &[msg_type]);
    put_option(out, &mut pos, OPT_SERVER_ID, &SERVER_OCTETS);
    put_option(out, &mut pos, OPT_LEASE_TIME, &LEASE_SECS.to_be_bytes());
    put_option(out, &mut pos, OPT_SUBNET_MASK, &SUBNET_MASK);
    put_option(out, &mut pos, OPT_ROUTER, &SERVER_OCTETS);
    put_option(out, &mut pos, OPT_DNS, &SERVER_OCTETS);
    out[pos] = OPT_END;
    pos += 1;
    pos
}

/// Build a DHCPNAK (used when we cannot satisfy a REQUEST). Returns length.
fn build_nak(req: &[u8], out: &mut [u8]) -> usize {
    build_reply(req, out, [0, 0, 0, 0], DHCPNAK)
}

/// The DHCP server task. Runs forever, even across the AP→STA switch — once we
/// leave AP mode no clients are associated, so it simply sits idle on recv.
#[embassy_executor::task]
pub async fn dhcp_server_task(stack: Stack<'static>) -> ! {
    // Generous buffers: DHCP packets are small but we keep room for options.
    let mut rx_meta = [PacketMetadata::EMPTY; 8];
    let mut rx_buffer = [0u8; 1024];
    let mut tx_meta = [PacketMetadata::EMPTY; 8];
    let mut tx_buffer = [0u8; 1024];
    let mut recv_buf = [0u8; 1024];
    let mut send_buf = [0u8; 1024];
    let mut leases = [Lease::EMPTY; POOL_SIZE];

    loop {
        let mut socket = UdpSocket::new(
            stack,
            &mut rx_meta,
            &mut rx_buffer,
            &mut tx_meta,
            &mut tx_buffer,
        );

        // Bind to 0.0.0.0:67 so we receive the client's broadcast DISCOVERs.
        if socket
            .bind(IpListenEndpoint {
                addr: None,
                port: DHCP_SERVER_PORT,
            })
            .is_err()
        {
            // Bind can fail transiently while the stack is reconfiguring; retry.
            Timer::after(Duration::from_millis(200)).await;
            continue;
        }

        loop {
            let (n, _meta) = match socket.recv_from(&mut recv_buf).await {
                Ok(v) => v,
                Err(_) => break, // re-create the socket on error
            };
            // Smallest valid DHCP: 240-byte BOOTP header + at least the cookie.
            if n < 240 || recv_buf[0] != OP_BOOTREQUEST {
                continue;
            }
            if recv_buf[236..240] != MAGIC_COOKIE {
                continue;
            }

            let mtype = match message_type(&recv_buf[240..n]) {
                Some(t) => t,
                None => continue,
            };

            // Client MAC from chaddr.
            let mut mac = [0u8; 6];
            mac.copy_from_slice(&recv_buf[28..34]);

            let reply_len = match mtype {
                DHCPDISCOVER => match allocate(&mut leases, &mac) {
                    Some(last) => {
                        let ip = [192, 168, 4, last];
                        build_reply(&recv_buf, &mut send_buf, ip, DHCPOFFER)
                    }
                    None => continue, // pool full: silently drop
                },
                DHCPREQUEST => match allocate(&mut leases, &mac) {
                    Some(last) => {
                        let ip = [192, 168, 4, last];
                        build_reply(&recv_buf, &mut send_buf, ip, DHCPACK)
                    }
                    None => build_nak(&recv_buf, &mut send_buf),
                },
                _ => continue, // ignore RELEASE/INFORM/DECLINE for now
            };

            // Reply by broadcast to 255.255.255.255:68. The client has no IP yet,
            // so unicast would be unreliable; broadcast is always accepted.
            let dest = IpEndpoint {
                addr: IpAddress::v4(255, 255, 255, 255),
                port: DHCP_CLIENT_PORT,
            };
            let _ = socket.send_to(&send_buf[..reply_len], dest).await;
        }
    }
}
