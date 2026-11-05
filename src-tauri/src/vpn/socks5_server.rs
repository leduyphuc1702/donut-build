use super::config::{VpnError, WireGuardConfig};
use super::tcp_shape::{self, OsStack, PacketShaper};
use boringtun::noise::{Tunn, TunnResult};
use boringtun::x25519::{PublicKey, StaticSecret};
use smoltcp::iface::{Config as IfaceConfig, Interface, PollResult, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::dns;
use smoltcp::socket::tcp::{Socket as TcpSocket, SocketBuffer};
use smoltcp::socket::udp;
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address};
use std::collections::VecDeque;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::{Arc, Mutex};
use tokio::net::{TcpListener, TcpStream};

/// One byte short of 64 KiB on purpose: smoltcp scales its advertised window by
/// `bit_length(capacity) - 16`, so 65535 keeps that shift at 0 and the window
/// field it writes is the real window in bytes. `tcp_shape` then scales it to
/// the shift the profile's OS announced in its SYN.
const SMOLTCP_TCP_RX_BUF: usize = 65535;
const SMOLTCP_TCP_TX_BUF: usize = 65536;
const SMOLTCP_UDP_BUF: usize = 65536;

/// The tunnel MTU when the WireGuard config sets none.
const DEFAULT_TUNNEL_MTU: usize = 1420;
const MIN_TUNNEL_MTU: usize = 1280;
const MAX_TUNNEL_MTU: usize = 1500;

/// The `[Interface] MTU` of the config, kept within what IPv6 and an Ethernet
/// link allow; 1420 when it sets none.
fn tunnel_mtu(configured: Option<u16>) -> usize {
  configured.map_or(DEFAULT_TUNNEL_MTU, |mtu| {
    usize::from(mtu).clamp(MIN_TUNNEL_MTU, MAX_TUNNEL_MTU)
  })
}

/// The per-socket half of an OS stack: its TTL, and TCP timestamps for an OS
/// that sends them, so smoltcp stamps every segment the way `tcp_shape` stamps
/// the SYN.
fn configure_tcp_socket(socket: &mut TcpSocket<'_>, os: OsStack) {
  socket.set_hop_limit(Some(os.ttl()));
  if os.timestamps() {
    socket.set_tsval_generator(Some(tcp_shape::tsval_now));
  }
}

/// How often boringtun's timers run: handshake renewal, keepalives.
const WG_TIMER_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// How long the relay loop waits before its next pass. A pass that moved
/// nothing doubles the wait, from 1 ms up to 8 ms, so an idle tunnel wakes
/// about 125 times a second instead of about 1000; any activity drops it back
/// to 1 ms. A new SOCKS5 connection ends the wait at once. Packets from the
/// peer and from the browser do not end it, so the cap is also the most an
/// idle tunnel adds to the first packet after a pause.
#[derive(Debug)]
struct IdleBackoff {
  wait_ms: u64,
}

impl IdleBackoff {
  const MIN_MS: u64 = 1;
  const MAX_MS: u64 = 8;

  fn new() -> Self {
    Self {
      wait_ms: Self::MIN_MS,
    }
  }

  /// The wait after a pass. `active` is whether the pass moved bytes,
  /// packets or connection state; `smoltcp_due` is when smoltcp next needs a
  /// poll (a retransmit, a DNS retry), which the wait never overshoots.
  fn next_wait(
    &mut self,
    active: bool,
    smoltcp_due: Option<std::time::Duration>,
  ) -> std::time::Duration {
    self.wait_ms = if active {
      Self::MIN_MS
    } else {
      (self.wait_ms * 2).min(Self::MAX_MS)
    };
    let wait = std::time::Duration::from_millis(self.wait_ms);
    smoltcp_due
      .map_or(wait, |due| wait.min(due))
      .max(std::time::Duration::from_millis(Self::MIN_MS))
  }
}

/// Parse an RFC 1928 §7 UDP request header. Returns the destination endpoint
/// and the payload offset, or None if malformed, fragmented, or domain-typed.
/// Only literal IPs are routed through the tunnel: resolving a domain on the
/// host would leak DNS, and QUIC/WebRTC datagrams always carry literal IPs.
fn parse_udp_datagram(buf: &[u8]) -> Option<(IpEndpoint, usize)> {
  if buf.len() < 4 || buf[2] != 0 {
    // too short, or FRAG != 0 (fragmentation unsupported)
    return None;
  }
  match buf[3] {
    0x01 => {
      if buf.len() < 10 {
        return None;
      }
      let ip = Ipv4Address::new(buf[4], buf[5], buf[6], buf[7]);
      let port = u16::from_be_bytes([buf[8], buf[9]]);
      Some((IpEndpoint::new(IpAddress::Ipv4(ip), port), 10))
    }
    0x04 => {
      if buf.len() < 22 {
        return None;
      }
      let mut o = [0u8; 16];
      o.copy_from_slice(&buf[4..20]);
      let ip = smoltcp::wire::Ipv6Address::from(o);
      let port = u16::from_be_bytes([buf[20], buf[21]]);
      Some((IpEndpoint::new(IpAddress::Ipv6(ip), port), 22))
    }
    _ => None,
  }
}

/// Wrap a tunnel-received datagram in an RFC 1928 §7 UDP reply header naming
/// `src` as the origin, for delivery back to the browser's relay socket.
fn build_udp_datagram(src: IpEndpoint, payload: &[u8]) -> Vec<u8> {
  let mut out = vec![0x00, 0x00, 0x00]; // RSV(2) + FRAG(0)
  match src.addr {
    IpAddress::Ipv4(v4) => {
      out.push(0x01);
      out.extend_from_slice(&v4.octets());
    }
    IpAddress::Ipv6(v6) => {
      out.push(0x04);
      out.extend_from_slice(&v6.octets());
    }
  }
  out.extend_from_slice(&src.port.to_be_bytes());
  out.extend_from_slice(payload);
  out
}

/// The IP packets between smoltcp and the tunnel. Packets leave it shaped to
/// the OS the profile claims and packets arriving are looked at first (see
/// `tcp_shape`). It is smoltcp's `Device`.
struct PacketPipe {
  rx_queue: VecDeque<Vec<u8>>,
  tx_queue: VecDeque<Vec<u8>>,
  shaper: PacketShaper,
  mtu: usize,
}

impl PacketPipe {
  fn new(os: OsStack, mtu: usize) -> Self {
    Self {
      rx_queue: VecDeque::new(),
      tx_queue: VecDeque::new(),
      shaper: PacketShaper::new(os, rand::random()),
      mtu,
    }
  }

  fn push_inbound(&mut self, packet: Vec<u8>) {
    self
      .shaper
      .observe_inbound(&packet, std::time::Instant::now());
    self.rx_queue.push_back(packet);
  }
}

struct WgDevice {
  tunn: Arc<Mutex<Box<Tunn>>>,
  udp_socket: Arc<UdpSocket>,
  peer_addr: SocketAddr,
  pipe: PacketPipe,
}

impl WgDevice {
  /// Decrypt every datagram the peer sent. Returns how many arrived.
  fn pump_wg_to_rx(&mut self) -> usize {
    let mut recv_buf = vec![0u8; 2048];
    let mut received = 0;
    loop {
      match self.udp_socket.recv_from(&mut recv_buf) {
        Ok((len, _)) => {
          received += 1;
          let mut dst = vec![0u8; 2048];
          let mut tunn = self.tunn.lock().unwrap();
          let result = tunn.decapsulate(None, &recv_buf[..len], &mut dst);
          match result {
            TunnResult::WriteToTunnelV4(data, _) | TunnResult::WriteToTunnelV6(data, _) => {
              self.pipe.push_inbound(data.to_vec());
            }
            TunnResult::WriteToNetwork(response) => {
              let _ = self.udp_socket.send_to(response, self.peer_addr);
            }
            _ => {}
          }
        }
        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
        Err(_) => break,
      }
    }
    received
  }

  /// Encrypt and send what smoltcp queued. Returns how many packets.
  fn flush_tx_queue(&mut self) -> usize {
    let mut sent = 0;
    while let Some(ip_packet) = self.pipe.tx_queue.pop_front() {
      sent += 1;
      let mut dst = vec![0u8; ip_packet.len() + 256];
      let mut tunn = self.tunn.lock().unwrap();
      let result = tunn.encapsulate(&ip_packet, &mut dst);
      match result {
        TunnResult::WriteToNetwork(packet) => {
          if let Err(e) = self.udp_socket.send_to(packet, self.peer_addr) {
            crate::proxy_server::warn_throttled(&format!("WireGuard UDP send failed err=\"{e}\""));
          }
        }
        TunnResult::Done => {
          // boringtun has nothing to send right now (e.g. handshake not yet
          // complete); silently drop. smoltcp will retransmit.
        }
        TunnResult::Err(e) => {
          crate::proxy_server::warn_throttled(&format!(
            "WireGuard encapsulate failed err=\"{e:?}\""
          ));
        }
        TunnResult::WriteToTunnelV4(_, _) | TunnResult::WriteToTunnelV6(_, _) => {
          crate::proxy_server::warn_throttled(
            "WireGuard encapsulate returned WriteToTunnel; unexpected, packet dropped",
          );
        }
      }
    }
    sent
  }

  fn tick_timers(&mut self) {
    let mut dst = vec![0u8; 2048];
    let mut tunn = self.tunn.lock().unwrap();
    let result = tunn.update_timers(&mut dst);
    if let TunnResult::WriteToNetwork(packet) = result {
      let _ = self.udp_socket.send_to(packet, self.peer_addr);
    }
  }
}

struct WgRxToken {
  data: Vec<u8>,
}

impl RxToken for WgRxToken {
  fn consume<R, F>(self, f: F) -> R
  where
    F: FnOnce(&[u8]) -> R,
  {
    f(&self.data)
  }
}

struct WgTxToken<'a> {
  tx_queue: &'a mut VecDeque<Vec<u8>>,
  shaper: &'a mut PacketShaper,
}

impl<'a> TxToken for WgTxToken<'a> {
  fn consume<R, F>(self, len: usize, f: F) -> R
  where
    F: FnOnce(&mut [u8]) -> R,
  {
    let mut buf = vec![0u8; len];
    let result = f(&mut buf);
    self
      .shaper
      .shape_outbound(&mut buf, std::time::Instant::now());
    self.tx_queue.push_back(buf);
    result
  }
}

impl Device for PacketPipe {
  type RxToken<'a> = WgRxToken;
  type TxToken<'a> = WgTxToken<'a>;

  fn receive(&mut self, _timestamp: SmolInstant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
    let data = self.rx_queue.pop_front()?;
    Some((
      WgRxToken { data },
      WgTxToken {
        tx_queue: &mut self.tx_queue,
        shaper: &mut self.shaper,
      },
    ))
  }

  fn transmit(&mut self, _timestamp: SmolInstant) -> Option<Self::TxToken<'_>> {
    Some(WgTxToken {
      tx_queue: &mut self.tx_queue,
      shaper: &mut self.shaper,
    })
  }

  fn capabilities(&self) -> DeviceCapabilities {
    let mut caps = DeviceCapabilities::default();
    caps.medium = Medium::Ip;
    caps.max_transmission_unit = self.mtu;
    caps
  }
}

fn parse_key(key: &str) -> Result<[u8; 32], VpnError> {
  let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, key)
    .map_err(|e| VpnError::InvalidWireGuard(format!("Invalid key encoding: {e}")))?;
  if decoded.len() != 32 {
    return Err(VpnError::InvalidWireGuard(format!(
      "Invalid key length: {} (expected 32)",
      decoded.len()
    )));
  }
  let mut key_bytes = [0u8; 32];
  key_bytes.copy_from_slice(&decoded);
  Ok(key_bytes)
}

fn parse_one_cidr(entry: &str) -> Result<(IpCidr, IpAddress), VpnError> {
  let parts: Vec<&str> = entry.split('/').collect();
  let ip_str = parts[0].trim();

  let ip: std::net::IpAddr = ip_str
    .parse()
    .map_err(|_| VpnError::InvalidWireGuard(format!("Invalid IP address: {ip_str}")))?;

  let prefix = if parts.len() > 1 {
    parts[1]
      .trim()
      .parse::<u8>()
      .map_err(|_| VpnError::InvalidWireGuard(format!("Invalid prefix length: {}", parts[1])))?
  } else if ip.is_ipv6() {
    128
  } else {
    32
  };

  match ip {
    std::net::IpAddr::V4(v4) => {
      let smol_ip = Ipv4Address::new(
        v4.octets()[0],
        v4.octets()[1],
        v4.octets()[2],
        v4.octets()[3],
      );
      Ok((
        IpCidr::new(IpAddress::Ipv4(smol_ip), prefix),
        IpAddress::Ipv4(smol_ip),
      ))
    }
    std::net::IpAddr::V6(v6) => {
      let smol_ip = smoltcp::wire::Ipv6Address::from(v6.octets());
      Ok((
        IpCidr::new(IpAddress::Ipv6(smol_ip), prefix),
        IpAddress::Ipv6(smol_ip),
      ))
    }
  }
}

/// Parse every address in a (comma-separated) WireGuard `Address` line. A
/// dual-stack config yields both the IPv4 and IPv6 entries so the tunnel
/// interface is addressed for each family it carries.
fn parse_cidr_addresses(addr: &str) -> Result<Vec<(IpCidr, IpAddress)>, VpnError> {
  let mut out = Vec::new();
  for entry in addr.split(',') {
    let entry = entry.trim();
    if entry.is_empty() {
      continue;
    }
    out.push(parse_one_cidr(entry)?);
  }
  if out.is_empty() {
    return Err(VpnError::InvalidWireGuard(format!(
      "No interface address in: {addr}"
    )));
  }
  Ok(out)
}

/// Convert a smoltcp `IpAddress` to a std `IpAddr`. Both smoltcp address types
/// are re-exports of `core::net`, so this is a straight variant map.
fn smol_to_std_ip(ip: IpAddress) -> std::net::IpAddr {
  match ip {
    IpAddress::Ipv4(v4) => std::net::IpAddr::V4(v4),
    IpAddress::Ipv6(v6) => std::net::IpAddr::V6(v6),
  }
}

pub struct WireGuardSocks5Server {
  config: WireGuardConfig,
  port: u16,
  /// The OS the profile claims; its TCP/IP stack is what the exit sees.
  os: OsStack,
}

impl WireGuardSocks5Server {
  pub fn new(config: WireGuardConfig, port: u16, os: OsStack) -> Self {
    Self { config, port, os }
  }

  fn create_tunnel(&self) -> Result<Box<Tunn>, VpnError> {
    let private_key_bytes = parse_key(&self.config.private_key)?;
    let static_private = StaticSecret::from(private_key_bytes);

    let peer_public_bytes = parse_key(&self.config.peer_public_key)?;
    let peer_public = PublicKey::from(peer_public_bytes);

    let preshared_key = if let Some(ref psk) = self.config.preshared_key {
      Some(parse_key(psk)?)
    } else {
      None
    };

    Ok(Box::new(Tunn::new(
      static_private,
      peer_public,
      preshared_key,
      self.config.persistent_keepalive,
      0,
      None,
    )))
  }

  fn resolve_endpoint(&self) -> Result<SocketAddr, VpnError> {
    self
      .config
      .peer_endpoint
      .to_socket_addrs()
      .map_err(|e| {
        VpnError::Connection(format!(
          "Failed to resolve endpoint '{}': {e}",
          self.config.peer_endpoint
        ))
      })?
      .next()
      .ok_or_else(|| {
        VpnError::Connection(format!(
          "No addresses found for endpoint: {}",
          self.config.peer_endpoint
        ))
      })
  }

  /// Returns how many attempts the handshake took.
  fn do_handshake(
    tunn: &mut Tunn,
    socket: &UdpSocket,
    peer_addr: SocketAddr,
  ) -> Result<u32, VpnError> {
    socket
      .set_read_timeout(Some(std::time::Duration::from_secs(5)))
      .map_err(|e| VpnError::Connection(format!("Failed to set timeout: {e}")))?;

    // WireGuard handshakes use UDP which can silently lose packets, especially
    // through Docker port-forwarding layers. Retry the handshake initiation up
    // to 5 times (25s total) before giving up — the protocol is designed for
    // retransmission and peers handle duplicate initiations correctly.
    let max_attempts = 5;
    let mut last_error = String::from("no handshake attempt completed");

    for attempt in 1..=max_attempts {
      let mut dst = vec![0u8; 2048];
      let result = tunn.format_handshake_initiation(&mut dst, false);

      match result {
        TunnResult::WriteToNetwork(packet) => {
          socket
            .send_to(packet, peer_addr)
            .map_err(|e| VpnError::Connection(format!("Failed to send handshake: {e}")))?;
        }
        TunnResult::Err(e) => {
          return Err(VpnError::Tunnel(format!(
            "Handshake initiation failed: {e:?}"
          )));
        }
        _ => {}
      }

      let mut recv_buf = vec![0u8; 2048];
      match socket.recv_from(&mut recv_buf) {
        Ok((len, _)) => {
          let result = tunn.decapsulate(None, &recv_buf[..len], &mut dst);
          match result {
            TunnResult::WriteToNetwork(response) => {
              socket
                .send_to(response, peer_addr)
                .map_err(|e| VpnError::Connection(format!("Failed to send response: {e}")))?;
            }
            TunnResult::Done => {}
            TunnResult::Err(e) => {
              last_error = format!("handshake response error: {e:?}");
              log::debug!(
                "WireGuard handshake attempt failed attempt={attempt}/{max_attempts} err=\"{last_error}\""
              );
              continue;
            }
            _ => {}
          }

          socket
            .set_read_timeout(None)
            .map_err(|e| VpnError::Connection(format!("Failed to clear timeout: {e}")))?;
          return Ok(attempt);
        }
        Err(e) if attempt < max_attempts => {
          log::debug!(
            "WireGuard handshake attempt timed out attempt={attempt}/{max_attempts} err=\"{e}\""
          );
          last_error = format!("timeout: {e}");
          continue;
        }
        Err(e) => {
          last_error = format!("timeout: {e}");
        }
      }
    }

    Err(VpnError::Connection(format!(
      "Handshake failed after {max_attempts} attempts: {last_error}"
    )))
  }

  pub async fn run(
    self,
    config_id: String,
    config_path: Option<std::path::PathBuf>,
  ) -> Result<(), VpnError> {
    let started = std::time::Instant::now();
    let peer_addr = self.resolve_endpoint()?;
    let mut tunn = self.create_tunnel()?;

    let udp_socket = UdpSocket::bind("0.0.0.0:0")
      .map_err(|e| VpnError::Connection(format!("Failed to create UDP socket: {e}")))?;

    let handshake_attempts = Self::do_handshake(&mut tunn, &udp_socket, peer_addr)?;

    udp_socket
      .set_nonblocking(true)
      .map_err(|e| VpnError::Connection(format!("Failed to set non-blocking: {e}")))?;

    let local_addrs = parse_cidr_addresses(&self.config.address)?;

    let tunn_arc = Arc::new(Mutex::new(tunn));
    let udp_arc = Arc::new(udp_socket);

    let os = self.os;
    let mtu = tunnel_mtu(self.config.mtu);
    let mut device = WgDevice {
      tunn: tunn_arc.clone(),
      udp_socket: udp_arc.clone(),
      peer_addr,
      pipe: PacketPipe::new(os, mtu),
    };

    let iface_config = IfaceConfig::new(HardwareAddress::Ip);
    let mut iface = Interface::new(iface_config, &mut device.pipe, SmolInstant::now());
    iface.update_ip_addrs(|addrs| {
      for (cidr, _) in &local_addrs {
        let _ = addrs.push(*cidr);
      }
    });

    // Install a default route for every address family the tunnel carries.
    // WireGuard is a routed (Medium::Ip) link, so the gateway is nominal — there
    // is no L2/neighbor resolution; smoltcp only uses it to select the default
    // route and then hands the packet straight to the WgDevice. The gateway is
    // derived from the interface address (x.x.x.1 / …::1) purely for form.
    let has_ipv4 = local_addrs
      .iter()
      .any(|(_, ip)| matches!(ip, IpAddress::Ipv4(_)));
    let has_ipv6 = local_addrs
      .iter()
      .any(|(_, ip)| matches!(ip, IpAddress::Ipv6(_)));
    for (_, ip) in &local_addrs {
      match ip {
        IpAddress::Ipv4(v4) => {
          let o = v4.octets();
          let gw = Ipv4Address::new(o[0], o[1], o[2], 1);
          iface
            .routes_mut()
            .add_default_ipv4_route(gw)
            .map_err(|e| VpnError::Tunnel(format!("Failed to add default IPv4 route: {e}")))?;
        }
        IpAddress::Ipv6(v6) => {
          let mut o = v6.octets();
          o[14] = 0;
          o[15] = 1;
          let gw = smoltcp::wire::Ipv6Address::from(o);
          iface
            .routes_mut()
            .add_default_ipv6_route(gw)
            .map_err(|e| VpnError::Tunnel(format!("Failed to add default IPv6 route: {e}")))?;
        }
      }
    }

    let listener = TcpListener::bind(format!("127.0.0.1:{}", self.port))
      .await
      .map_err(|e| VpnError::Connection(format!("Failed to bind SOCKS5 listener: {e}")))?;

    let actual_port = listener
      .local_addr()
      .map_err(|e| VpnError::Connection(format!("Failed to get local addr: {e}")))?
      .port();

    // Update config with actual port and local_url. Prefer the explicit
    // config path the worker was started with — a case where
    // get_storage_dir() in the worker process resolved to a different
    // directory than in the parent (Qubes/sandboxed Linux), causing the
    // write-back to land in the wrong place and the parent to time out.
    let updated = match &config_path {
      Some(path) => crate::vpn_worker_storage::get_vpn_worker_config_from_path(path)
        .or_else(|| crate::vpn_worker_storage::get_vpn_worker_config(&config_id)),
      None => crate::vpn_worker_storage::get_vpn_worker_config(&config_id),
    };
    let vpn_id = updated
      .as_ref()
      .map_or_else(|| "-".to_string(), |wc| wc.vpn_id.clone());
    if let Some(mut wc) = updated {
      wc.local_port = Some(actual_port);
      wc.local_url = Some(format!("socks5://127.0.0.1:{}", actual_port));
      let result = match &config_path {
        Some(path) => crate::vpn_worker_storage::save_vpn_worker_config_to_path(&wc, path)
          .map_err(|e| e.to_string()),
        None => crate::vpn_worker_storage::save_vpn_worker_config(&wc).map_err(|e| e.to_string()),
      };
      if let Err(e) = result {
        log::error!(
          "Worker config write-back failed; the app will not see this port id={config_id} path={:?} err=\"{e}\"",
          config_path
        );
      }
    } else {
      log::error!(
        "Worker config write-back skipped: config not found id={config_id} path={:?}",
        config_path
      );
    }

    log::info!(
      "Worker started kind=vpn id={config_id} pid={} listen=127.0.0.1:{actual_port} vpn={vpn_id} vpn_type=wireguard os={} mtu={mtu} peer={peer_addr} handshake_attempts={handshake_attempts} elapsed_ms={}",
      std::process::id(),
      os.as_str(),
      started.elapsed().as_millis()
    );

    // DNS resolution for domain-name CONNECT requests must go THROUGH the tunnel, never
    // the host resolver (which would leak the query to the local network — the
    // whole point of the VPN). Resolve via the WireGuard config's DNS server
    // (default 1.1.1.1, still routed through the tunnel). `dns_servers` must
    // outlive `sockets` since the socket borrows it, so declare it first.
    let dns_servers = [self
      .config
      .dns
      .as_deref()
      .and_then(|s| s.trim().parse::<std::net::Ipv4Addr>().ok())
      .map(|v4| {
        let o = v4.octets();
        IpAddress::Ipv4(Ipv4Address::new(o[0], o[1], o[2], o[3]))
      })
      .unwrap_or_else(|| IpAddress::Ipv4(Ipv4Address::new(1, 1, 1, 1)))];

    let mut sockets = SocketSet::new(vec![]);

    // Single DNS socket shared by all connections; queries are driven through
    // the tunnel by iface.poll like every other socket. Build the query storage
    // without vec![None; N] since DnsQuery isn't Clone.
    let dns_queries: Vec<Option<dns::DnsQuery>> = (0..64).map(|_| None).collect();
    let mut dns_socket = dns::Socket::new(&dns_servers, dns_queries);
    dns_socket.set_hop_limit(Some(os.ttl()));
    let dns_handle = sockets.add(dns_socket);

    // A live SOCKS5 UDP ASSOCIATE: the loopback relay socket the browser sends
    // datagrams to, and the browser's learned source address. The tunnel-side
    // smoltcp UDP socket lives in `sockets`, keyed by the connection's
    // (repurposed) `smol_handle`.
    struct UdpAssoc {
      relay: UdpSocket,
      client_addr: Option<SocketAddr>,
    }

    // An in-flight through-tunnel DNS resolution for a domain-name CONNECT.
    struct PendingDns {
      query: dns::QueryHandle,
      port: u16,
      domain: String,
      // true if this is an AAAA query (resolving to IPv6); false for A.
      want_ipv6: bool,
      // whether we've already retried with the other address family.
      fell_back: bool,
    }

    struct Connection {
      smol_handle: SocketHandle,
      tcp_stream: TcpStream,
      socks_done: bool,
      connecting: bool,
      greeting_done: bool,
      read_buf: Vec<u8>,
      dest_addr: Option<SocketAddr>,
      udp: Option<UdpAssoc>,
      pending_dns: Option<PendingDns>,
    }

    let mut connections: Vec<Connection> = Vec::new();
    let mut idle = IdleBackoff::new();
    let mut last_timer_tick = std::time::Instant::now();
    let mut accepted: Option<TcpStream> = None;

    loop {
      // Whether this pass moved bytes, packets or connection state. A pass
      // that did not lets the loop sleep longer.
      let mut active = false;

      // A SOCKS5 connection the wait below accepted.
      if let Some(stream) = accepted.take() {
        active = true;
        let tcp_rx = SocketBuffer::new(vec![0u8; SMOLTCP_TCP_RX_BUF]);
        let tcp_tx = SocketBuffer::new(vec![0u8; SMOLTCP_TCP_TX_BUF]);
        let mut tcp_socket = TcpSocket::new(tcp_rx, tcp_tx);
        configure_tcp_socket(&mut tcp_socket, os);
        let handle = sockets.add(tcp_socket);

        connections.push(Connection {
          smol_handle: handle,
          tcp_stream: stream,
          socks_done: false,
          connecting: false,
          greeting_done: false,
          read_buf: Vec::new(),
          dest_addr: None,
          udp: None,
          pending_dns: None,
        });
      }

      // Pump WireGuard packets into smoltcp rx queue
      if device.pump_wg_to_rx() > 0 {
        active = true;
      }

      // Poll the smoltcp interface
      let timestamp = SmolInstant::now();
      if iface.poll(timestamp, &mut device.pipe, &mut sockets) == PollResult::SocketStateChanged {
        active = true;
      }

      // Flush encrypted packets out through WireGuard
      if device.flush_tx_queue() > 0 {
        active = true;
      }

      // Process each connection
      let mut completed = Vec::new();
      for (idx, conn) in connections.iter_mut().enumerate() {
        if let Some(pending) = conn.pending_dns.take() {
          // A through-tunnel DNS resolution is in flight for this connection.
          let result = {
            let dns_sock = sockets.get_mut::<dns::Socket>(dns_handle);
            dns_sock.get_query_result(pending.query)
          };

          // Classify: a usable address of the queried family, a clean "no such
          // record", still-pending, or a hard failure.
          enum DnsOutcome {
            Resolved(IpAddress),
            NoRecord,
            Pending,
            Failed,
          }
          let outcome = match result {
            Ok(addrs) => {
              let want_ipv6 = pending.want_ipv6;
              match addrs.iter().copied().find(|a| {
                matches!(
                  (a, want_ipv6),
                  (IpAddress::Ipv4(_), false) | (IpAddress::Ipv6(_), true)
                )
              }) {
                Some(ip) => DnsOutcome::Resolved(ip),
                None => DnsOutcome::NoRecord,
              }
            }
            Err(dns::GetQueryResultError::Pending) => DnsOutcome::Pending,
            Err(_) => DnsOutcome::Failed,
          };

          match outcome {
            DnsOutcome::Pending => {
              // Still resolving; put it back and check again next poll.
              conn.pending_dns = Some(pending);
            }
            DnsOutcome::Resolved(ip) => {
              active = true;
              conn.dest_addr = Some(SocketAddr::new(smol_to_std_ip(ip), pending.port));
              let socket = sockets.get_mut::<TcpSocket>(conn.smol_handle);
              let local_port = 10000 + (rand::random::<u16>() % 50000);
              if socket
                .connect(iface.context(), (ip, pending.port), local_port)
                .is_err()
              {
                let _ = conn
                  .tcp_stream
                  .try_write(&[0x05, 0x05, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
                completed.push(idx);
                continue;
              }
              conn.connecting = true;
            }
            DnsOutcome::NoRecord | DnsOutcome::Failed => {
              active = true;
              // No record for this family (or the query failed). Retry once with
              // the other family if the tunnel carries it — a dual-stack name
              // that only had the other record, or an IPv4-preferred query for
              // an IPv6-only name.
              let other_ipv6 = !pending.want_ipv6;
              let other_supported = if other_ipv6 { has_ipv6 } else { has_ipv4 };
              let mut requeued = false;
              if !pending.fell_back && other_supported {
                let qtype = if other_ipv6 {
                  smoltcp::wire::DnsQueryType::Aaaa
                } else {
                  smoltcp::wire::DnsQueryType::A
                };
                let dns_sock = sockets.get_mut::<dns::Socket>(dns_handle);
                if let Ok(query) = dns_sock.start_query(iface.context(), &pending.domain, qtype) {
                  conn.pending_dns = Some(PendingDns {
                    query,
                    port: pending.port,
                    domain: pending.domain,
                    want_ipv6: other_ipv6,
                    fell_back: true,
                  });
                  requeued = true;
                }
              }
              if !requeued {
                let _ = conn
                  .tcp_stream
                  .try_write(&[0x05, 0x04, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
                completed.push(idx);
                continue;
              }
            }
          }
        } else if conn.connecting {
          let socket = sockets.get_mut::<TcpSocket>(conn.smol_handle);
          if socket.may_send() {
            active = true;
            let _ = conn.tcp_stream.try_write(&[
              0x05,
              0x00,
              0x00,
              0x01,
              127,
              0,
              0,
              1,
              (actual_port >> 8) as u8,
              (actual_port & 0xff) as u8,
            ]);
            conn.connecting = false;
            conn.socks_done = true;
          } else if !socket.is_open() {
            let _ = conn
              .tcp_stream
              .try_write(&[0x05, 0x05, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
            completed.push(idx);
          }
        } else if !conn.socks_done {
          // Handle SOCKS5 handshake
          let mut buf = [0u8; 512];
          match conn.tcp_stream.try_read(&mut buf) {
            Ok(0) => {
              completed.push(idx);
              continue;
            }
            Ok(n) => {
              active = true;
              conn.read_buf.extend_from_slice(&buf[..n]);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => {
              completed.push(idx);
              continue;
            }
          }

          if !conn.greeting_done && conn.read_buf.len() >= 3 {
            // SOCKS5 greeting: version, nmethods, methods
            if conn.read_buf[0] != 0x05 {
              completed.push(idx);
              continue;
            }
            let nmethods = conn.read_buf[1] as usize;
            if conn.read_buf.len() < 2 + nmethods {
              continue;
            }
            // Reply: no auth required
            if conn.tcp_stream.try_write(&[0x05, 0x00]).is_err() {
              completed.push(idx);
              continue;
            }
            conn.read_buf.drain(..2 + nmethods);
            conn.greeting_done = true;
          }

          if conn.greeting_done && conn.dest_addr.is_none() && conn.read_buf.len() >= 10 {
            // SOCKS5 request: CONNECT (0x01) or UDP ASSOCIATE (0x03)
            if conn.read_buf[0] != 0x05 {
              completed.push(idx);
              continue;
            }
            let cmd = conn.read_buf[1];
            if cmd != 0x01 && cmd != 0x03 {
              // command not supported
              let _ = conn
                .tcp_stream
                .try_write(&[0x05, 0x07, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
              completed.push(idx);
              continue;
            }

            let (addr, addr_len) = match conn.read_buf[3] {
              0x01 => {
                // IPv4
                if conn.read_buf.len() < 10 {
                  continue;
                }
                let ip = std::net::Ipv4Addr::new(
                  conn.read_buf[4],
                  conn.read_buf[5],
                  conn.read_buf[6],
                  conn.read_buf[7],
                );
                let port = u16::from_be_bytes([conn.read_buf[8], conn.read_buf[9]]);
                (SocketAddr::new(std::net::IpAddr::V4(ip), port), 10)
              }
              0x03 => {
                // Domain name
                let domain_len = conn.read_buf[4] as usize;
                let needed = 4 + 1 + domain_len + 2;
                if conn.read_buf.len() < needed {
                  continue;
                }
                let domain = String::from_utf8_lossy(&conn.read_buf[5..5 + domain_len]).to_string();
                let port_start = 5 + domain_len;
                let port =
                  u16::from_be_bytes([conn.read_buf[port_start], conn.read_buf[port_start + 1]]);
                if cmd == 0x03 {
                  // UDP ASSOCIATE ignores the DST address (the relay learns the
                  // client's source from its first datagram), so no resolution
                  // is needed — never resolve it on the host.
                  (
                    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), port),
                    needed,
                  )
                } else {
                  // TCP CONNECT: resolve the domain THROUGH THE TUNNEL via the
                  // config DNS server, never the host resolver (which would leak
                  // the query onto the local network). Prefer IPv4 when the
                  // tunnel carries it (the well-tested path) and fall back to
                  // AAAA only for names with no A record; an IPv6-only tunnel
                  // resolves AAAA directly. Start an async query and defer the
                  // connection until it resolves.
                  conn.read_buf.drain(..needed);
                  let want_ipv6 = !has_ipv4;
                  let qtype = if want_ipv6 {
                    smoltcp::wire::DnsQueryType::Aaaa
                  } else {
                    smoltcp::wire::DnsQueryType::A
                  };
                  let dns_sock = sockets.get_mut::<dns::Socket>(dns_handle);
                  match dns_sock.start_query(iface.context(), &domain, qtype) {
                    Ok(query) => {
                      conn.pending_dns = Some(PendingDns {
                        query,
                        port,
                        domain,
                        want_ipv6,
                        fell_back: false,
                      });
                    }
                    Err(e) => {
                      crate::proxy_server::warn_throttled(&format!(
                        "Tunnel DNS query start failed err=\"{e:?}\""
                      ));
                      let _ = conn
                        .tcp_stream
                        .try_write(&[0x05, 0x04, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
                      completed.push(idx);
                    }
                  }
                  continue;
                }
              }
              0x04 => {
                // IPv6-literal CONNECT. Route it through the tunnel when the
                // tunnel carries IPv6; otherwise it can never be reached (no
                // IPv6 address/route), so fail fast with "host unreachable"
                // rather than letting the smoltcp socket sit in SynSent until it
                // times out (a visible hang).
                if conn.read_buf.len() < 22 {
                  continue;
                }
                if !has_ipv6 {
                  let _ = conn
                    .tcp_stream
                    .try_write(&[0x05, 0x04, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
                  completed.push(idx);
                  continue;
                }
                let mut octets = [0u8; 16];
                octets.copy_from_slice(&conn.read_buf[4..20]);
                let ip = std::net::Ipv6Addr::from(octets);
                let port = u16::from_be_bytes([conn.read_buf[20], conn.read_buf[21]]);
                (SocketAddr::new(std::net::IpAddr::V6(ip), port), 22)
              }
              _ => {
                completed.push(idx);
                continue;
              }
            };

            conn.read_buf.drain(..addr_len);

            if cmd == 0x03 {
              // === SOCKS5 UDP ASSOCIATE ===
              // The request's DST is the client's intended source (typically
              // 0.0.0.0:0) and is ignored — the browser's relay source is
              // learned from its first datagram. Bind a loopback relay socket
              // the browser sends to, plus a smoltcp UDP socket that egresses
              // through the WireGuard tunnel on the interface IP.
              let relay = match UdpSocket::bind("127.0.0.1:0") {
                Ok(s) => s,
                Err(_) => {
                  let _ = conn
                    .tcp_stream
                    .try_write(&[0x05, 0x01, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
                  completed.push(idx);
                  continue;
                }
              };
              let _ = relay.set_nonblocking(true);
              let relay_port = relay.local_addr().map(|a| a.port()).unwrap_or(0);

              // Reply with the relay endpoint (127.0.0.1:relay_port).
              if conn
                .tcp_stream
                .try_write(&[
                  0x05,
                  0x00,
                  0x00,
                  0x01,
                  127,
                  0,
                  0,
                  1,
                  (relay_port >> 8) as u8,
                  (relay_port & 0xff) as u8,
                ])
                .is_err()
              {
                completed.push(idx);
                continue;
              }

              let udp_rx = udp::PacketBuffer::new(
                vec![udp::PacketMetadata::EMPTY; 32],
                vec![0u8; SMOLTCP_UDP_BUF],
              );
              let udp_tx = udp::PacketBuffer::new(
                vec![udp::PacketMetadata::EMPTY; 32],
                vec![0u8; SMOLTCP_UDP_BUF],
              );
              let mut udp_socket = udp::Socket::new(udp_rx, udp_tx);
              udp_socket.set_hop_limit(Some(os.ttl()));
              let local_port = 20000 + (rand::random::<u16>() % 40000);
              if udp_socket.bind(local_port).is_err() {
                completed.push(idx);
                continue;
              }

              // Swap this connection's unused TCP socket for the UDP socket;
              // `smol_handle` now keys the UDP socket, so teardown is unchanged.
              sockets.remove(conn.smol_handle);
              conn.smol_handle = sockets.add(udp_socket);
              conn.udp = Some(UdpAssoc {
                relay,
                client_addr: None,
              });
              conn.socks_done = true;
              continue;
            }

            conn.dest_addr = Some(addr);

            // Open smoltcp TCP socket to the destination
            let socket = sockets.get_mut::<TcpSocket>(conn.smol_handle);
            let smol_addr = match addr.ip() {
              std::net::IpAddr::V4(v4) => {
                let o = v4.octets();
                IpAddress::Ipv4(Ipv4Address::new(o[0], o[1], o[2], o[3]))
              }
              std::net::IpAddr::V6(v6) => {
                IpAddress::Ipv6(smoltcp::wire::Ipv6Address::from(v6.octets()))
              }
            };

            let local_port = 10000 + (rand::random::<u16>() % 50000);
            if socket
              .connect(iface.context(), (smol_addr, addr.port()), local_port)
              .is_err()
            {
              let _ = conn
                .tcp_stream
                .try_write(&[0x05, 0x05, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
              completed.push(idx);
              continue;
            }

            conn.connecting = true;
          }
        } else if conn.udp.is_some() {
          // === UDP ASSOCIATE relay ===
          // The association lives only while the TCP control connection is
          // open (RFC 1928 §6); tear down when the browser closes it.
          let mut probe = [0u8; 1];
          match conn.tcp_stream.try_read(&mut probe) {
            Ok(0) => {
              completed.push(idx);
              continue;
            }
            Ok(_) => {} // ignore any data on the control channel
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => {
              completed.push(idx);
              continue;
            }
          }

          let handle = conn.smol_handle;
          let Some(udp) = conn.udp.as_mut() else {
            continue;
          };

          // Browser → tunnel: strip the §7 header and forward the payload.
          let mut dbuf = [0u8; SMOLTCP_UDP_BUF];
          loop {
            match udp.relay.recv_from(&mut dbuf) {
              Ok((n, src)) => {
                active = true;
                udp.client_addr = Some(src);
                if let Some((dst, off)) = parse_udp_datagram(&dbuf[..n]) {
                  let socket = sockets.get_mut::<udp::Socket>(handle);
                  if socket.can_send() {
                    let _ = socket.send_slice(&dbuf[off..n], dst);
                  }
                }
              }
              Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
              Err(_) => break,
            }
          }

          // Tunnel → browser: wrap each datagram in a §7 header and relay back.
          loop {
            let socket = sockets.get_mut::<udp::Socket>(handle);
            if !socket.can_recv() {
              break;
            }
            let (payload, src) = match socket.recv() {
              Ok((data, meta)) => (data.to_vec(), meta.endpoint),
              Err(_) => break,
            };
            active = true;
            if let Some(client) = udp.client_addr {
              let resp = build_udp_datagram(src, &payload);
              let _ = udp.relay.send_to(&resp, client);
            }
          }
        } else {
          // Data relay between SOCKS5 client and smoltcp socket. Bytes move
          // only as far as the other side takes them: what smoltcp has no
          // room for stays in the client socket, and what the client cannot
          // take yet stays in smoltcp, so a full buffer never drops data.
          let socket = sockets.get_mut::<TcpSocket>(conn.smol_handle);

          // Client → smoltcp, read straight into smoltcp's free space.
          if socket.can_send() {
            let read = socket.send(|free| {
              if free.is_empty() {
                return (0, Err(std::io::ErrorKind::WouldBlock.into()));
              }
              match conn.tcp_stream.try_read(free) {
                Ok(n) => (n, Ok(n)),
                Err(e) => (0, Err(e)),
              }
            });
            match read {
              Ok(Ok(0)) => {
                socket.close();
                completed.push(idx);
                continue;
              }
              Ok(Ok(_)) => active = true,
              Ok(Err(ref e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
              Ok(Err(_)) | Err(_) => {
                socket.close();
                completed.push(idx);
                continue;
              }
            }
          }

          // smoltcp → Client: dequeue only what the client socket took. Two
          // rounds cover a receive buffer that wraps around.
          let mut write_failed = false;
          for _ in 0..2 {
            if !socket.can_recv() {
              break;
            }
            let written = socket.recv(|data| match conn.tcp_stream.try_write(data) {
              Ok(n) => (n, Ok(n)),
              Err(e) => (0, Err(e)),
            });
            match written {
              Ok(Ok(0)) => break,
              Ok(Ok(_)) => active = true,
              Ok(Err(ref e)) if e.kind() == std::io::ErrorKind::WouldBlock => break,
              Ok(Err(_)) => {
                write_failed = true;
                break;
              }
              Err(_) => break,
            }
          }
          if write_failed {
            socket.close();
            completed.push(idx);
            continue;
          }

          // Check if smoltcp socket closed
          if !socket.is_open() && !socket.is_active() {
            completed.push(idx);
          }
        }
      }

      // Remove completed connections (in reverse order)
      if !completed.is_empty() {
        active = true;
      }
      completed.sort_unstable();
      completed.dedup();
      for idx in completed.into_iter().rev() {
        let conn = connections.remove(idx);
        if let Some(pending) = conn.pending_dns {
          sockets
            .get_mut::<dns::Socket>(dns_handle)
            .cancel_query(pending.query);
        }
        sockets.remove(conn.smol_handle);
      }

      // Timer ticks for WireGuard keepalives
      if last_timer_tick.elapsed() >= WG_TIMER_INTERVAL {
        device.tick_timers();
        last_timer_tick = std::time::Instant::now();
      }

      // Wait for the next pass: briefly while traffic flows, longer while the
      // tunnel is idle, never past a smoltcp or WireGuard timer, and no
      // longer than it takes a new SOCKS5 connection to arrive.
      let smoltcp_due = iface
        .poll_delay(SmolInstant::now(), &sockets)
        .map(|due| std::time::Duration::from_millis(due.total_millis()));
      let timer_due = WG_TIMER_INTERVAL.saturating_sub(last_timer_tick.elapsed());
      let due = Some(smoltcp_due.map_or(timer_due, |due| due.min(timer_due)));
      let wait = idle.next_wait(active, due);
      tokio::select! {
        result = listener.accept() => match result {
          Ok((stream, _addr)) => accepted = Some(stream),
          Err(e) => {
            crate::proxy_server::warn_throttled(&format!("SOCKS5 accept failed err=\"{e}\""));
            tokio::time::sleep(wait).await;
          }
        },
        _ = tokio::time::sleep(wait) => {}
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn new_socket(rx: usize) -> TcpSocket<'static> {
    TcpSocket::new(
      SocketBuffer::new(vec![0u8; rx]),
      SocketBuffer::new(vec![0u8; 64]),
    )
  }

  #[test]
  fn a_socket_takes_the_ttl_and_timestamps_of_its_os() {
    for (os, ttl, timestamps) in [
      (OsStack::Windows, 128, false),
      (OsStack::MacOs, 64, true),
      (OsStack::Linux, 64, true),
    ] {
      let mut socket = new_socket(16);
      configure_tcp_socket(&mut socket, os);
      assert_eq!(socket.hop_limit(), Some(ttl), "{os:?}");
      assert_eq!(socket.timestamp_enabled(), timestamps, "{os:?}");
    }
  }

  #[test]
  fn the_receive_buffer_keeps_smoltcps_window_shift_at_zero() {
    assert_eq!(
      usize::BITS - SMOLTCP_TCP_RX_BUF.leading_zeros(),
      16,
      "a 16-bit buffer length means no internal window scaling"
    );
    assert!(SMOLTCP_TCP_RX_BUF <= usize::from(u16::MAX));
  }

  #[test]
  fn the_tunnel_mtu_follows_the_config_within_bounds() {
    assert_eq!(tunnel_mtu(None), 1420);
    assert_eq!(tunnel_mtu(Some(1380)), 1380);
    assert_eq!(tunnel_mtu(Some(1280)), 1280);
    assert_eq!(tunnel_mtu(Some(1500)), 1500);
    assert_eq!(tunnel_mtu(Some(576)), 1280, "below the IPv6 minimum");
    assert_eq!(tunnel_mtu(Some(9000)), 1500, "above Ethernet");
    assert_eq!(tunnel_mtu(Some(0)), 1280);

    let pipe = PacketPipe::new(OsStack::Linux, tunnel_mtu(Some(1360)));
    assert_eq!(pipe.capabilities().max_transmission_unit, 1360);
  }

  struct Side {
    iface: Interface,
    pipe: PacketPipe,
    sockets: SocketSet<'static>,
    handle: SocketHandle,
  }

  impl Side {
    fn new(os: OsStack, mtu: usize, ip: IpAddress, rx_buffer: usize) -> Self {
      let mut pipe = PacketPipe::new(os, mtu);
      let mut iface = Interface::new(
        IfaceConfig::new(HardwareAddress::Ip),
        &mut pipe,
        SmolInstant::from_millis(0),
      );
      let prefix = if matches!(ip, IpAddress::Ipv4(_)) {
        24
      } else {
        64
      };
      iface.update_ip_addrs(|addrs| {
        let _ = addrs.push(IpCidr::new(ip, prefix));
      });
      let mut sockets = SocketSet::new(vec![]);
      let mut socket = TcpSocket::new(
        SocketBuffer::new(vec![0u8; rx_buffer]),
        SocketBuffer::new(vec![0u8; SMOLTCP_TCP_TX_BUF]),
      );
      configure_tcp_socket(&mut socket, os);
      let handle = sockets.add(socket);
      Self {
        iface,
        pipe,
        sockets,
        handle,
      }
    }

    fn socket(&mut self) -> &mut TcpSocket<'static> {
      self.sockets.get_mut::<TcpSocket>(self.handle)
    }
  }

  fn pattern(offset: usize) -> u8 {
    (offset ^ (offset >> 8) ^ (offset >> 16)) as u8
  }

  /// What a client segment says, after smoltcp's parser has checked its length
  /// and both checksums.
  struct Seen {
    syn: bool,
    ack: bool,
    window: u16,
    options: Vec<u8>,
    ttl: u8,
  }

  fn inspect(packet: &[u8]) -> Seen {
    use smoltcp::wire::{Ipv4Packet, Ipv6Packet, TcpPacket};
    let (src, dst, ttl, payload) = if packet[0] >> 4 == 4 {
      let ip = Ipv4Packet::new_checked(packet).expect("ipv4");
      assert!(ip.verify_checksum(), "ipv4 header checksum");
      assert!(ip.dont_frag(), "DF");
      assert_ne!(ip.ident(), 0, "ip id");
      (
        IpAddress::Ipv4(ip.src_addr()),
        IpAddress::Ipv4(ip.dst_addr()),
        ip.hop_limit(),
        ip.payload(),
      )
    } else {
      let ip = Ipv6Packet::new_checked(packet).expect("ipv6");
      (
        IpAddress::Ipv6(ip.src_addr()),
        IpAddress::Ipv6(ip.dst_addr()),
        ip.hop_limit(),
        ip.payload(),
      )
    };
    let tcp = TcpPacket::new_checked(payload).expect("tcp");
    assert!(tcp.verify_checksum(&src, &dst), "tcp checksum");
    Seen {
      syn: tcp.syn(),
      ack: tcp.ack(),
      window: tcp.window_len(),
      options: tcp.options().to_vec(),
      ttl,
    }
  }

  /// A client of `os` and a plain peer run a real TCP connection back to back
  /// through `PacketPipe`, each sending `total` bytes to the other. The
  /// transfer finishing intact proves the window rewrite never stalls or
  /// corrupts a connection; the inspected segments prove what the peer saw.
  fn transfer(os: OsStack, v6: bool, mtu: usize, total: usize) {
    let (client_ip, server_ip) = if v6 {
      (
        IpAddress::Ipv6(smoltcp::wire::Ipv6Address::new(0xfd00, 0, 0, 0, 0, 0, 0, 2)),
        IpAddress::Ipv6(smoltcp::wire::Ipv6Address::new(0xfd00, 0, 0, 0, 0, 0, 0, 1)),
      )
    } else {
      (
        IpAddress::Ipv4(Ipv4Address::new(10, 0, 0, 2)),
        IpAddress::Ipv4(Ipv4Address::new(10, 0, 0, 1)),
      )
    };
    let mut client = Side::new(os, mtu, client_ip, SMOLTCP_TCP_RX_BUF);
    // A peer with a larger buffer, so its SYN-ACK scales by a different shift.
    let mut server = Side::new(OsStack::Linux, mtu, server_ip, 262_144);
    server.socket().listen(443).unwrap();
    let handle = client.handle;
    client
      .sockets
      .get_mut::<TcpSocket>(handle)
      .connect(client.iface.context(), (server_ip, 443), 40000)
      .unwrap();

    let ip_header = if v6 { 40 } else { 20 };
    let expected_mss = (mtu - ip_header - 20) as u16;
    let mut syn_seen = None;
    let mut max_window_after_syn = 0u16;
    let (mut sent_c, mut recv_c, mut sent_s, mut recv_s) = (0usize, 0usize, 0usize, 0usize);
    let mut finished_at = None;

    for step in 0..60_000i64 {
      let now = SmolInstant::from_millis(step);
      client
        .iface
        .poll(now, &mut client.pipe, &mut client.sockets);
      while let Some(packet) = client.pipe.tx_queue.pop_front() {
        let seen = inspect(&packet);
        assert_eq!(seen.ttl, os.ttl());
        if seen.syn {
          assert!(!seen.ack);
          syn_seen.get_or_insert(seen);
        } else if seen.ack {
          max_window_after_syn = max_window_after_syn.max(seen.window);
        }
        server.pipe.push_inbound(packet);
      }
      server
        .iface
        .poll(now, &mut server.pipe, &mut server.sockets);
      while let Some(packet) = server.pipe.tx_queue.pop_front() {
        client.pipe.push_inbound(packet);
      }

      for (side, sent, received) in [
        (&mut client, &mut sent_c, &mut recv_c),
        (&mut server, &mut sent_s, &mut recv_s),
      ] {
        let socket = side.socket();
        if socket.may_send() && *sent < total {
          let from = *sent;
          let n = socket
            .send(|buf| {
              let n = buf.len().min(total - from);
              for (i, byte) in buf[..n].iter_mut().enumerate() {
                *byte = pattern(from + i);
              }
              (n, n)
            })
            .unwrap();
          *sent += n;
        }
        if socket.can_recv() {
          let from = *received;
          let n = socket
            .recv(|data| {
              for (i, byte) in data.iter().enumerate() {
                assert_eq!(*byte, pattern(from + i), "corrupt byte at {}", from + i);
              }
              (data.len(), data.len())
            })
            .unwrap();
          *received += n;
        }
      }
      if (sent_c, recv_c, sent_s, recv_s) == (total, total, total, total) {
        finished_at = Some(step);
        break;
      }
    }

    let finished_at = finished_at.unwrap_or_else(|| {
      panic!(
        "{os:?} v6={v6}: stalled, client sent {sent_c} got {recv_c}, server sent {sent_s} got {recv_s}"
      )
    });
    assert!(
      finished_at < 30_000,
      "{os:?} v6={v6}: took {finished_at} simulated ms"
    );

    let syn = syn_seen.expect("the client sent a SYN");
    assert_eq!(syn.window, os.syn_window(), "{os:?} SYN window");
    assert_eq!(syn.options.len(), os.syn_options(expected_mss, 0).len());
    assert_eq!(
      &syn.options[..4],
      &[2, 4, (expected_mss >> 8) as u8, expected_mss as u8]
    );
    if os == OsStack::Windows {
      assert_eq!(syn.options, os.syn_options(expected_mss, 0));
    }

    let cap = u16::MAX >> os.window_shift();
    assert!(
      (1..=cap).contains(&max_window_after_syn),
      "{os:?}: scaled window field {max_window_after_syn} must be within 1..={cap}"
    );
  }

  #[test]
  fn a_windows_client_moves_5_mb_each_way_over_ipv4() {
    transfer(OsStack::Windows, false, 1420, 5 * 1024 * 1024);
  }

  #[test]
  fn a_macos_client_moves_5_mb_each_way_over_ipv4() {
    transfer(OsStack::MacOs, false, 1420, 5 * 1024 * 1024);
  }

  #[test]
  fn a_linux_client_moves_5_mb_each_way_over_ipv4() {
    transfer(OsStack::Linux, false, 1420, 5 * 1024 * 1024);
  }

  #[test]
  fn every_os_moves_data_over_ipv6_at_the_smallest_mtu() {
    for os in [OsStack::Windows, OsStack::MacOs, OsStack::Linux] {
      transfer(os, true, 1280, 1024 * 1024);
    }
  }

  #[test]
  fn test_parse_cidr_ipv4() {
    let (cidr, ip) = parse_one_cidr("10.0.0.2/24").unwrap();
    assert_eq!(cidr.prefix_len(), 24);
    assert_eq!(ip, IpAddress::Ipv4(Ipv4Address::new(10, 0, 0, 2)));
  }

  #[test]
  fn test_parse_cidr_no_prefix() {
    let (cidr, _) = parse_one_cidr("10.0.0.2").unwrap();
    assert_eq!(cidr.prefix_len(), 32);
  }

  #[test]
  fn test_parse_cidr_ipv6_default_prefix() {
    let (cidr, ip) = parse_one_cidr("fd00::2").unwrap();
    assert_eq!(cidr.prefix_len(), 128);
    assert!(matches!(ip, IpAddress::Ipv6(_)));
  }

  #[test]
  fn test_parse_cidr_addresses_dual_stack() {
    let addrs = parse_cidr_addresses("10.0.0.2/24, fd00::2/128").unwrap();
    assert_eq!(addrs.len(), 2);
    assert_eq!(addrs[0].1, IpAddress::Ipv4(Ipv4Address::new(10, 0, 0, 2)));
    assert!(matches!(addrs[1].1, IpAddress::Ipv6(_)));
    assert!(addrs.iter().any(|(_, ip)| matches!(ip, IpAddress::Ipv4(_))));
    assert!(addrs.iter().any(|(_, ip)| matches!(ip, IpAddress::Ipv6(_))));
  }

  #[test]
  fn test_parse_key_valid() {
    let key = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    assert!(parse_key(key).is_ok());
  }

  #[test]
  fn test_parse_key_invalid() {
    assert!(parse_key("not-valid").is_err());
  }

  #[test]
  fn an_idle_loop_backs_off_and_activity_resets_it() {
    use std::time::Duration;
    let ms = Duration::from_millis;
    let mut idle = IdleBackoff::new();
    assert_eq!(idle.next_wait(true, None), ms(1), "traffic keeps 1 ms");
    let waits: Vec<_> = (0..5).map(|_| idle.next_wait(false, None)).collect();
    assert_eq!(
      waits,
      vec![ms(2), ms(4), ms(8), ms(8), ms(8)],
      "an idle tunnel doubles up to 8 ms"
    );
    assert_eq!(idle.next_wait(true, None), ms(1), "activity drops it back");
    assert_eq!(idle.next_wait(false, None), ms(2));
  }

  #[test]
  fn the_backoff_never_sleeps_past_a_timer_or_spins() {
    use std::time::Duration;
    let ms = Duration::from_millis;
    let mut idle = IdleBackoff::new();
    for _ in 0..10 {
      idle.next_wait(false, None);
    }
    assert_eq!(idle.next_wait(false, Some(ms(5))), ms(5), "a due timer");
    assert_eq!(
      idle.next_wait(false, Some(Duration::ZERO)),
      ms(1),
      "a timer due now still waits 1 ms, so the loop never spins"
    );
    assert_eq!(idle.next_wait(false, Some(ms(500))), ms(8));
  }
}
