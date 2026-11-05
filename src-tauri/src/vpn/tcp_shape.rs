//! Per-profile OS TCP/IP fingerprint shaping for the in-app WireGuard worker.
//!
//! The worker builds the browser's outbound TCP itself (smoltcp) and the
//! WireGuard exit only NATs it, so a site sees the SYN smoltcp produced:
//! window 65535, options `MSS, WS, SACK_PERM, EOL`, no NOPs, TTL 64. No real OS
//! sends that. smoltcp's public API cannot change the SYN window, the option
//! order or the window scale, so [`PacketShaper`] rewrites the finished IP
//! packets just before they are encrypted, to the layout of the OS the
//! profile claims ([`OsStack`]).
//!
//! What it changes:
//! - every outbound packet: TTL / hop limit, the IPv4 DF bit and a non-zero
//!   IPv4 identification (real stacks set DF together with a non-zero ID; smoltcp
//!   sends ID 0);
//! - outbound TCP SYN: window and the whole options block, in the OS order;
//! - every later segment of a connection whose peer agreed to window scaling:
//!   the window field, divided by the scale the SYN advertised. smoltcp itself
//!   runs unscaled (its receive buffer is 65535 bytes, shift 0), so the
//!   field it writes is the real window in bytes.
//!
//! The module touches nothing but byte buffers, so it is tested without a
//! tunnel.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const IPV4_MIN_HEADER: usize = 20;
const IPV6_HEADER: usize = 40;
const TCP_MIN_HEADER: usize = 20;
const PROTO_TCP: u8 = 6;

const TCP_FIN: u8 = 0x01;
const TCP_SYN: u8 = 0x02;
const TCP_RST: u8 = 0x04;
const TCP_ACK: u8 = 0x10;

const OPT_EOL: u8 = 0;
const OPT_NOP: u8 = 1;
const OPT_MSS: u8 = 2;
const OPT_WINDOW_SCALE: u8 = 3;
const OPT_SACK_PERMITTED: u8 = 4;
const OPT_TIMESTAMP: u8 = 8;

/// A connection that has been idle this long stops being tracked. smoltcp keeps
/// no keep-alive, so only a connection that sat silent for an hour loses its
/// window scaling (and the peer then reads an unscaled window as larger than
/// it is, which the receive buffer still bounds).
const FLOW_IDLE_TTL: Duration = Duration::from_secs(3600);
/// How long a connection is tracked after both sides sent FIN, for the last
/// ACKs.
const FLOW_CLOSE_LINGER: Duration = Duration::from_secs(30);
/// A browser holds a few hundred connections; this only bounds a leak.
const MAX_FLOWS: usize = 16_384;
/// Packets between two sweeps of the flow table.
const SWEEP_EVERY: u32 = 512;

/// The OS whose TCP/IP stack a profile's packets must look like.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OsStack {
  Windows,
  MacOs,
  Linux,
}

impl OsStack {
  /// `windows`, `macos` or `linux` in any case; anything else is `None`.
  pub fn parse(name: &str) -> Option<Self> {
    match name.trim().to_ascii_lowercase().as_str() {
      "windows" => Some(Self::Windows),
      "macos" => Some(Self::MacOs),
      "linux" => Some(Self::Linux),
      _ => None,
    }
  }

  /// The OS this machine runs.
  pub fn host() -> Self {
    if cfg!(target_os = "macos") {
      Self::MacOs
    } else if cfg!(target_os = "windows") {
      Self::Windows
    } else {
      Self::Linux
    }
  }

  pub fn as_str(self) -> &'static str {
    match self {
      Self::Windows => "windows",
      Self::MacOs => "macos",
      Self::Linux => "linux",
    }
  }

  /// The TTL / hop limit this OS starts packets with.
  pub const fn ttl(self) -> u8 {
    match self {
      Self::Windows => 128,
      Self::MacOs | Self::Linux => 64,
    }
  }

  /// The window field of this OS's SYN.
  pub const fn syn_window(self) -> u16 {
    match self {
      Self::Windows | Self::Linux => 64240,
      Self::MacOs => 65535,
    }
  }

  /// The window scale this OS's SYN advertises.
  pub const fn window_shift(self) -> u8 {
    match self {
      Self::Windows => 8,
      Self::MacOs => 6,
      Self::Linux => 7,
    }
  }

  /// Whether this OS sends TCP timestamps. Sockets of such an OS get
  /// [`tsval_now`] as their timestamp generator, so smoltcp stamps every
  /// segment of the connection and not only the SYN.
  pub const fn timestamps(self) -> bool {
    !matches!(self, Self::Windows)
  }

  /// The options block of this OS's SYN. `mss` is what the tunnel MTU allows
  /// (kept from smoltcp, never the textbook 1460); `tsval` is used by the
  /// timestamp OSes only.
  pub fn syn_options(self, mss: u16, tsval: u32) -> Vec<u8> {
    let mss = mss.to_be_bytes();
    let shift = self.window_shift();
    let ts = tsval.to_be_bytes();
    match self {
      Self::Windows => vec![
        OPT_MSS,
        4,
        mss[0],
        mss[1],
        OPT_NOP,
        OPT_WINDOW_SCALE,
        3,
        shift,
        OPT_NOP,
        OPT_NOP,
        OPT_SACK_PERMITTED,
        2,
      ],
      Self::MacOs => vec![
        OPT_MSS,
        4,
        mss[0],
        mss[1],
        OPT_NOP,
        OPT_WINDOW_SCALE,
        3,
        shift,
        OPT_NOP,
        OPT_NOP,
        OPT_TIMESTAMP,
        10,
        ts[0],
        ts[1],
        ts[2],
        ts[3],
        0,
        0,
        0,
        0,
        OPT_SACK_PERMITTED,
        2,
        OPT_EOL,
        OPT_EOL,
      ],
      Self::Linux => vec![
        OPT_MSS,
        4,
        mss[0],
        mss[1],
        OPT_SACK_PERMITTED,
        2,
        OPT_TIMESTAMP,
        10,
        ts[0],
        ts[1],
        ts[2],
        ts[3],
        0,
        0,
        0,
        0,
        OPT_NOP,
        OPT_WINDOW_SCALE,
        3,
        shift,
      ],
    }
  }
}

/// The timestamp clock of every shaped socket: milliseconds from a random
/// per-process origin. It is a plain `fn` because smoltcp's generator type is.
pub fn tsval_now() -> u32 {
  static CLOCK: OnceLock<(Instant, u32)> = OnceLock::new();
  let (start, base) = CLOCK.get_or_init(|| (Instant::now(), rand::random::<u32>()));
  base.wrapping_add(start.elapsed().as_millis() as u32)
}

/// A window field for a connection that agreed to window scaling: the real
/// window in bytes divided by `2^shift`. The result is zero only when fewer
/// than `2^shift` bytes are free, which is all a scaled window can express.
pub fn scale_window(real: u16, shift: u8) -> u16 {
  real >> shift.min(14)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FlowKey {
  local_port: u16,
  remote: IpAddr,
  remote_port: u16,
}

/// A connection whose peer agreed to window scaling.
#[derive(Debug)]
struct Flow {
  shift: u8,
  last_seen: Instant,
  fin_in: bool,
  fin_out: bool,
  closing_since: Option<Instant>,
}

impl Flow {
  fn note_fin(&mut self, outbound: bool, now: Instant) {
    if outbound {
      self.fin_out = true;
    } else {
      self.fin_in = true;
    }
    if self.fin_in && self.fin_out && self.closing_since.is_none() {
      self.closing_since = Some(now);
    }
  }
}

/// What the IP header of a packet says; offsets all land inside the packet.
#[derive(Debug, Clone, Copy)]
struct IpInfo {
  v6: bool,
  header_len: usize,
  /// End of the IP datagram (IPv4 total length, IPv6 40 + payload length).
  end: usize,
  proto: u8,
  /// A non-first or non-last IPv4 fragment; its payload is not a TCP header.
  fragment: bool,
}

fn be16(bytes: &[u8]) -> u16 {
  u16::from_be_bytes([bytes[0], bytes[1]])
}

fn parse_ip(packet: &[u8]) -> Option<IpInfo> {
  match packet.first()? >> 4 {
    4 => {
      if packet.len() < IPV4_MIN_HEADER {
        return None;
      }
      let header_len = usize::from(packet[0] & 0x0f) * 4;
      let end = usize::from(be16(&packet[2..4]));
      if header_len < IPV4_MIN_HEADER || end < header_len || end > packet.len() {
        return None;
      }
      Some(IpInfo {
        v6: false,
        header_len,
        end,
        proto: packet[9],
        fragment: be16(&packet[6..8]) & 0x3fff != 0,
      })
    }
    6 => {
      if packet.len() < IPV6_HEADER {
        return None;
      }
      let end = IPV6_HEADER + usize::from(be16(&packet[4..6]));
      if end > packet.len() {
        return None;
      }
      Some(IpInfo {
        v6: true,
        header_len: IPV6_HEADER,
        end,
        proto: packet[6],
        fragment: false,
      })
    }
    _ => None,
  }
}

fn flow_addrs(packet: &[u8], ip: &IpInfo) -> Option<(IpAddr, IpAddr)> {
  if ip.v6 {
    let src = <[u8; 16]>::try_from(&packet[8..24]).ok()?;
    let dst = <[u8; 16]>::try_from(&packet[24..40]).ok()?;
    Some((
      IpAddr::V6(Ipv6Addr::from(src)),
      IpAddr::V6(Ipv6Addr::from(dst)),
    ))
  } else {
    let src = <[u8; 4]>::try_from(&packet[12..16]).ok()?;
    let dst = <[u8; 4]>::try_from(&packet[16..20]).ok()?;
    Some((
      IpAddr::V4(Ipv4Addr::from(src)),
      IpAddr::V4(Ipv4Addr::from(dst)),
    ))
  }
}

fn checksum_sum(data: &[u8], mut acc: u64) -> u64 {
  let mut bytes = data.iter().copied();
  while let Some(high) = bytes.next() {
    let low = bytes.next().unwrap_or(0);
    acc += u64::from(u16::from_be_bytes([high, low]));
  }
  acc
}

fn checksum_fold(mut acc: u64) -> u16 {
  while acc >> 16 != 0 {
    acc = (acc & 0xffff) + (acc >> 16);
  }
  !(acc as u16)
}

fn refresh_ipv4_checksum(packet: &mut [u8], header_len: usize) {
  packet[10] = 0;
  packet[11] = 0;
  let sum = checksum_fold(checksum_sum(&packet[..header_len], 0));
  packet[10..12].copy_from_slice(&sum.to_be_bytes());
}

/// Recompute the TCP checksum of `packet[l4..end]`, pseudo-header included.
fn refresh_tcp_checksum(packet: &mut [u8], v6: bool, l4: usize, end: usize) {
  packet[l4 + 16] = 0;
  packet[l4 + 17] = 0;
  let addresses = if v6 { 8..40 } else { 12..20 };
  let tcp_len = (end - l4) as u64;
  let mut acc = checksum_sum(&packet[addresses], 0);
  acc += u64::from(PROTO_TCP) + (tcp_len >> 16) + (tcp_len & 0xffff);
  acc = checksum_sum(&packet[l4..end], acc);
  let sum = checksum_fold(acc);
  packet[l4 + 16..l4 + 18].copy_from_slice(&sum.to_be_bytes());
}

#[derive(Debug, Default, PartialEq, Eq)]
struct TcpOptions {
  mss: Option<u16>,
  window_scale: Option<u8>,
  tsval: Option<u32>,
}

fn parse_tcp_options(mut options: &[u8]) -> TcpOptions {
  let mut found = TcpOptions::default();
  while let Some(&kind) = options.first() {
    match kind {
      OPT_EOL => break,
      OPT_NOP => options = &options[1..],
      _ => {
        let Some(&len) = options.get(1) else { break };
        let len = usize::from(len);
        if len < 2 || len > options.len() {
          break;
        }
        let body = &options[2..len];
        match (kind, len) {
          (OPT_MSS, 4) => found.mss = Some(be16(body)),
          (OPT_WINDOW_SCALE, 3) => found.window_scale = Some(body[0]),
          (OPT_TIMESTAMP, 10) => {
            found.tsval = Some(u32::from_be_bytes([body[0], body[1], body[2], body[3]]));
          }
          _ => {}
        }
        options = &options[len..];
      }
    }
  }
  found
}

/// The TCP header length and options of a packet whose TCP part is at least
/// 20 bytes, or `None` when the data offset points outside the datagram.
fn tcp_options_of<'a>(packet: &'a [u8], ip: &IpInfo) -> Option<&'a [u8]> {
  let l4 = ip.header_len;
  if ip.end < l4 + TCP_MIN_HEADER {
    return None;
  }
  let header = usize::from(packet[l4 + 12] >> 4) * 4;
  if header < TCP_MIN_HEADER || l4 + header > ip.end {
    return None;
  }
  Some(&packet[l4 + TCP_MIN_HEADER..l4 + header])
}

/// Rewrites outbound packets to one OS's stack and tracks which connections
/// agreed to window scaling.
#[derive(Debug)]
pub struct PacketShaper {
  os: OsStack,
  flows: HashMap<FlowKey, Flow>,
  next_ip_id: u16,
  packets_since_sweep: u32,
}

impl PacketShaper {
  /// `ip_id_seed` starts the IPv4 identification counter; a worker passes a
  /// random value.
  pub fn new(os: OsStack, ip_id_seed: u16) -> Self {
    Self {
      os,
      flows: HashMap::new(),
      next_ip_id: ip_id_seed.max(1),
      packets_since_sweep: 0,
    }
  }

  pub fn os(&self) -> OsStack {
    self.os
  }

  #[cfg(test)]
  fn tracked_flows(&self) -> usize {
    self.flows.len()
  }

  /// Shape a packet smoltcp emitted, right before it is encrypted. The packet
  /// may change length (a SYN gets its OS's option block).
  pub fn shape_outbound(&mut self, packet: &mut Vec<u8>, now: Instant) {
    let Some(ip) = parse_ip(packet) else {
      return;
    };
    self.tick(now);
    if ip.proto == PROTO_TCP && !ip.fragment {
      self.shape_tcp(packet, &ip, now);
    }
    self.stamp_ip(packet);
  }

  /// Look at a packet the peer sent, before smoltcp does: a SYN-ACK that
  /// carries a window scale option makes its connection a scaled one.
  pub fn observe_inbound(&mut self, packet: &[u8], now: Instant) {
    let Some(ip) = parse_ip(packet) else {
      return;
    };
    if ip.proto != PROTO_TCP || ip.fragment || ip.end < ip.header_len + TCP_MIN_HEADER {
      return;
    }
    self.tick(now);
    let l4 = ip.header_len;
    let Some((remote, _)) = flow_addrs(packet, &ip) else {
      return;
    };
    let key = FlowKey {
      local_port: be16(&packet[l4 + 2..l4 + 4]),
      remote,
      remote_port: be16(&packet[l4..l4 + 2]),
    };
    let flags = packet[l4 + 13];
    if flags & TCP_RST != 0 {
      self.flows.remove(&key);
    } else if flags & (TCP_SYN | TCP_ACK) == (TCP_SYN | TCP_ACK) {
      let scaled = tcp_options_of(packet, &ip)
        .is_some_and(|options| parse_tcp_options(options).window_scale.is_some());
      if scaled {
        self.insert_flow(key, now);
      } else {
        self.flows.remove(&key);
      }
    } else if let Some(flow) = self.flows.get_mut(&key) {
      flow.last_seen = now;
      if flags & TCP_FIN != 0 {
        flow.note_fin(false, now);
      }
    }
  }

  fn tick(&mut self, now: Instant) {
    self.packets_since_sweep += 1;
    if self.packets_since_sweep >= SWEEP_EVERY {
      self.sweep(now);
    }
  }

  fn sweep(&mut self, now: Instant) {
    self.packets_since_sweep = 0;
    self.flows.retain(|_, flow| {
      let idle = now.saturating_duration_since(flow.last_seen) >= FLOW_IDLE_TTL;
      let lingered = flow
        .closing_since
        .is_some_and(|since| now.saturating_duration_since(since) >= FLOW_CLOSE_LINGER);
      !idle && !lingered
    });
  }

  fn insert_flow(&mut self, key: FlowKey, now: Instant) {
    if self.flows.len() >= MAX_FLOWS && !self.flows.contains_key(&key) {
      self.sweep(now);
      if self.flows.len() >= MAX_FLOWS {
        let oldest = self
          .flows
          .iter()
          .min_by_key(|(_, flow)| flow.last_seen)
          .map(|(key, _)| *key);
        if let Some(oldest) = oldest {
          self.flows.remove(&oldest);
        }
      }
    }
    self.flows.insert(
      key,
      Flow {
        shift: self.os.window_shift(),
        last_seen: now,
        fin_in: false,
        fin_out: false,
        closing_since: None,
      },
    );
  }

  fn shape_tcp(&mut self, packet: &mut Vec<u8>, ip: &IpInfo, now: Instant) {
    let l4 = ip.header_len;
    if ip.end < l4 + TCP_MIN_HEADER {
      return;
    }
    let Some((_, remote)) = flow_addrs(packet, ip) else {
      return;
    };
    let key = FlowKey {
      local_port: be16(&packet[l4..l4 + 2]),
      remote,
      remote_port: be16(&packet[l4 + 2..l4 + 4]),
    };
    let flags = packet[l4 + 13];

    if flags & (TCP_SYN | TCP_ACK) == TCP_SYN {
      self.flows.remove(&key);
      self.rewrite_syn(packet, ip);
      return;
    }
    if flags & TCP_RST != 0 {
      self.flows.remove(&key);
      return;
    }
    let Some(flow) = self.flows.get_mut(&key) else {
      return;
    };
    flow.last_seen = now;
    if flags & TCP_FIN != 0 {
      flow.note_fin(true, now);
    }
    let real = be16(&packet[l4 + 14..l4 + 16]);
    let field = scale_window(real, flow.shift);
    if field != real {
      packet[l4 + 14..l4 + 16].copy_from_slice(&field.to_be_bytes());
      refresh_tcp_checksum(packet, ip.v6, l4, ip.end);
    }
  }

  /// Replace a SYN's window and options with this OS's, keeping the MSS (and
  /// the timestamp value) smoltcp chose.
  fn rewrite_syn(&self, packet: &mut Vec<u8>, ip: &IpInfo) {
    let l4 = ip.header_len;
    let Some(old_options) = tcp_options_of(packet, ip) else {
      return;
    };
    let old_header = TCP_MIN_HEADER + old_options.len();
    let found = parse_tcp_options(old_options);
    let Some(mss) = found.mss else {
      return;
    };
    let options = self
      .os
      .syn_options(mss, found.tsval.unwrap_or_else(tsval_now));
    debug_assert_eq!(options.len() % 4, 0);

    let mut out = Vec::with_capacity(l4 + TCP_MIN_HEADER + options.len() + ip.end);
    out.extend_from_slice(&packet[..l4 + TCP_MIN_HEADER]);
    out.extend_from_slice(&options);
    out.extend_from_slice(&packet[l4 + old_header..ip.end]);

    let header = TCP_MIN_HEADER + options.len();
    out[l4 + 12] = (((header / 4) as u8) << 4) | (out[l4 + 12] & 0x0f);
    out[l4 + 14..l4 + 16].copy_from_slice(&self.os.syn_window().to_be_bytes());

    let end = out.len();
    if ip.v6 {
      out[4..6].copy_from_slice(&((end - IPV6_HEADER) as u16).to_be_bytes());
    } else {
      out[2..4].copy_from_slice(&(end as u16).to_be_bytes());
    }
    refresh_tcp_checksum(&mut out, ip.v6, l4, end);
    *packet = out;
  }

  /// TTL / hop limit on every packet, and on IPv4 the DF bit and a non-zero ID.
  fn stamp_ip(&mut self, packet: &mut [u8]) {
    if packet[0] >> 4 == 6 {
      packet[7] = self.os.ttl();
      return;
    }
    let header_len = usize::from(packet[0] & 0x0f) * 4;
    packet[8] = self.os.ttl();
    if be16(&packet[6..8]) & 0x3fff == 0 {
      packet[6] |= 0x40;
      if be16(&packet[4..6]) == 0 {
        let id = self.next_ip_id;
        self.next_ip_id = id.checked_add(1).unwrap_or(1);
        packet[4..6].copy_from_slice(&id.to_be_bytes());
      }
    }
    refresh_ipv4_checksum(packet, header_len);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use smoltcp::phy::ChecksumCapabilities;
  use smoltcp::wire::{
    IpAddress, IpProtocol, IpRepr, Ipv4Packet, Ipv6Packet, TcpControl, TcpPacket, TcpRepr,
    TcpSeqNumber, TcpTimestampRepr,
  };

  const MSS_V4: u16 = 1380;
  const MSS_V6: u16 = 1360;
  const TSVAL: u32 = 0x1122_3344;

  fn v4(last: u8) -> IpAddress {
    IpAddress::Ipv4(Ipv4Addr::new(10, 0, 0, last))
  }

  fn v6(last: u16) -> IpAddress {
    IpAddress::Ipv6(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, last))
  }

  fn emit(src: IpAddress, dst: IpAddress, repr: &TcpRepr, hop_limit: u8) -> Vec<u8> {
    let ip = IpRepr::new(src, dst, IpProtocol::Tcp, repr.buffer_len(), hop_limit);
    let mut buf = vec![0u8; ip.buffer_len()];
    let caps = ChecksumCapabilities::default();
    ip.emit(&mut buf[..], &caps);
    let header = ip.header_len();
    repr.emit(
      &mut TcpPacket::new_unchecked(&mut buf[header..]),
      &src,
      &dst,
      &caps,
    );
    buf
  }

  /// A segment as smoltcp builds it.
  fn segment<'a>(control: TcpControl, ack: bool, window: u16, payload: &'a [u8]) -> TcpRepr<'a> {
    let syn = control == TcpControl::Syn;
    TcpRepr {
      src_port: 40000,
      dst_port: 443,
      control,
      seq_number: TcpSeqNumber(1000),
      ack_number: ack.then_some(TcpSeqNumber(77)),
      window_len: window,
      window_scale: syn.then_some(0),
      max_seg_size: None,
      sack_permitted: syn && !ack,
      sack_ranges: [None, None, None],
      timestamp: None,
      payload,
    }
  }

  fn smoltcp_syn(src: IpAddress, dst: IpAddress, mss: u16, timestamp: bool) -> Vec<u8> {
    let mut repr = segment(TcpControl::Syn, false, 65535, &[]);
    repr.max_seg_size = Some(mss);
    repr.timestamp = timestamp.then(|| TcpTimestampRepr::new(TSVAL, 0));
    emit(src, dst, &repr, 64)
  }

  fn peer_syn_ack(local: IpAddress, remote: IpAddress, scale: Option<u8>) -> Vec<u8> {
    let mut repr = segment(TcpControl::Syn, true, 29200, &[]);
    repr.src_port = 443;
    repr.dst_port = 40000;
    repr.window_scale = scale;
    repr.max_seg_size = Some(1380);
    emit(remote, local, &repr, 56)
  }

  fn shaped(os: OsStack, mut packet: Vec<u8>) -> Vec<u8> {
    PacketShaper::new(os, 0x4000).shape_outbound(&mut packet, Instant::now());
    packet
  }

  fn header_len(packet: &[u8]) -> usize {
    parse_ip(packet).unwrap().header_len
  }

  fn options(packet: &[u8]) -> &[u8] {
    let l4 = header_len(packet);
    let tcp_header = usize::from(packet[l4 + 12] >> 4) * 4;
    &packet[l4 + TCP_MIN_HEADER..l4 + tcp_header]
  }

  fn window(packet: &[u8]) -> u16 {
    let l4 = header_len(packet);
    be16(&packet[l4 + 14..l4 + 16])
  }

  /// Checks every length and checksum with smoltcp's own parser, which does not
  /// share code with this module, and returns the parsed TCP part.
  fn assert_wire_valid(packet: &[u8]) -> TcpRepr<'_> {
    let caps = ChecksumCapabilities::default();
    let (src, dst, tcp_bytes) = match packet[0] >> 4 {
      4 => {
        let ip = Ipv4Packet::new_checked(packet).expect("ipv4 header");
        assert!(ip.verify_checksum(), "ipv4 header checksum");
        assert_eq!(
          usize::from(ip.total_len()),
          packet.len(),
          "ipv4 total length"
        );
        (
          IpAddress::Ipv4(ip.src_addr()),
          IpAddress::Ipv4(ip.dst_addr()),
          ip.payload(),
        )
      }
      _ => {
        let ip = Ipv6Packet::new_checked(packet).expect("ipv6 header");
        assert_eq!(
          IPV6_HEADER + usize::from(ip.payload_len()),
          packet.len(),
          "ipv6 payload length"
        );
        (
          IpAddress::Ipv6(ip.src_addr()),
          IpAddress::Ipv6(ip.dst_addr()),
          ip.payload(),
        )
      }
    };
    let tcp = TcpPacket::new_checked(tcp_bytes).expect("tcp header");
    assert!(tcp.verify_checksum(&src, &dst), "tcp checksum");
    TcpRepr::parse(&tcp, &src, &dst, &caps).expect("smoltcp parses the rewritten segment")
  }

  #[test]
  fn each_os_has_its_ttl_window_and_scale() {
    assert_eq!(OsStack::Windows.ttl(), 128);
    assert_eq!(OsStack::MacOs.ttl(), 64);
    assert_eq!(OsStack::Linux.ttl(), 64);
    assert_eq!(OsStack::Windows.syn_window(), 64240);
    assert_eq!(OsStack::MacOs.syn_window(), 65535);
    assert_eq!(OsStack::Linux.syn_window(), 64240);
    assert_eq!(OsStack::Windows.window_shift(), 8);
    assert_eq!(OsStack::MacOs.window_shift(), 6);
    assert_eq!(OsStack::Linux.window_shift(), 7);
    assert!(!OsStack::Windows.timestamps());
    assert!(OsStack::MacOs.timestamps());
    assert!(OsStack::Linux.timestamps());
  }

  #[test]
  fn os_names_parse_and_unknown_names_do_not() {
    assert_eq!(OsStack::parse("windows"), Some(OsStack::Windows));
    assert_eq!(OsStack::parse(" MacOS "), Some(OsStack::MacOs));
    assert_eq!(OsStack::parse("Linux"), Some(OsStack::Linux));
    assert_eq!(OsStack::parse("android"), None);
    assert_eq!(OsStack::parse(""), None);
    for os in [OsStack::Windows, OsStack::MacOs, OsStack::Linux] {
      assert_eq!(OsStack::parse(os.as_str()), Some(os));
    }
    assert_eq!(
      OsStack::host().as_str(),
      crate::profile::types::get_host_os()
    );
  }

  #[test]
  fn the_option_blocks_are_the_exact_bytes_of_each_os() {
    assert_eq!(
      OsStack::Windows.syn_options(MSS_V4, TSVAL),
      [0x02, 0x04, 0x05, 0x64, 0x01, 0x03, 0x03, 0x08, 0x01, 0x01, 0x04, 0x02]
    );
    assert_eq!(
      OsStack::MacOs.syn_options(MSS_V4, TSVAL),
      [
        0x02, 0x04, 0x05, 0x64, 0x01, 0x03, 0x03, 0x06, 0x01, 0x01, 0x08, 0x0a, 0x11, 0x22, 0x33,
        0x44, 0x00, 0x00, 0x00, 0x00, 0x04, 0x02, 0x00, 0x00
      ]
    );
    assert_eq!(
      OsStack::Linux.syn_options(MSS_V4, TSVAL),
      [
        0x02, 0x04, 0x05, 0x64, 0x04, 0x02, 0x08, 0x0a, 0x11, 0x22, 0x33, 0x44, 0x00, 0x00, 0x00,
        0x00, 0x01, 0x03, 0x03, 0x07
      ]
    );
    assert_eq!(OsStack::Windows.syn_options(1, 0).len(), 12);
    assert_eq!(OsStack::MacOs.syn_options(1, 0).len(), 24);
    assert_eq!(OsStack::Linux.syn_options(1, 0).len(), 20);
  }

  #[test]
  fn a_windows_syn_is_rewritten_to_the_exact_stack() {
    let out = shaped(OsStack::Windows, smoltcp_syn(v4(2), v4(9), MSS_V4, false));
    assert_eq!(
      options(&out),
      [0x02, 0x04, 0x05, 0x64, 0x01, 0x03, 0x03, 0x08, 0x01, 0x01, 0x04, 0x02]
    );
    assert_eq!(window(&out), 64240);
    assert_eq!(out[8], 128, "ttl");
    assert_ne!(out[6] & 0x40, 0, "DF");
    assert_ne!(be16(&out[4..6]), 0, "ip id");
    assert_eq!(out.len(), 20 + 20 + 12);
    let parsed = assert_wire_valid(&out);
    assert_eq!(parsed.max_seg_size, Some(MSS_V4));
    assert_eq!(parsed.window_scale, Some(8));
    assert!(parsed.sack_permitted);
    assert_eq!(parsed.timestamp, None);
  }

  #[test]
  fn a_macos_syn_grows_and_carries_the_smoltcp_timestamp() {
    let input = smoltcp_syn(v4(2), v4(9), MSS_V4, true);
    assert_eq!(options(&input).len(), 20, "smoltcp's own SYN options");
    let out = shaped(OsStack::MacOs, input.clone());
    assert_eq!(
      options(&out),
      [
        0x02, 0x04, 0x05, 0x64, 0x01, 0x03, 0x03, 0x06, 0x01, 0x01, 0x08, 0x0a, 0x11, 0x22, 0x33,
        0x44, 0x00, 0x00, 0x00, 0x00, 0x04, 0x02, 0x00, 0x00
      ]
    );
    assert_eq!(window(&out), 65535);
    assert_eq!(out[8], 64);
    assert_eq!(out.len(), input.len() + 4, "four bytes longer");
    let parsed = assert_wire_valid(&out);
    assert_eq!(parsed.window_scale, Some(6));
    assert_eq!(parsed.timestamp, Some(TcpTimestampRepr::new(TSVAL, 0)));
    assert!(parsed.sack_permitted);
  }

  #[test]
  fn a_linux_syn_is_rewritten_to_the_exact_stack() {
    let out = shaped(OsStack::Linux, smoltcp_syn(v4(2), v4(9), MSS_V4, true));
    assert_eq!(
      options(&out),
      [
        0x02, 0x04, 0x05, 0x64, 0x04, 0x02, 0x08, 0x0a, 0x11, 0x22, 0x33, 0x44, 0x00, 0x00, 0x00,
        0x00, 0x01, 0x03, 0x03, 0x07
      ]
    );
    assert_eq!(window(&out), 64240);
    assert_eq!(out[8], 64);
    let parsed = assert_wire_valid(&out);
    assert_eq!(parsed.window_scale, Some(7));
    assert_eq!(parsed.timestamp, Some(TcpTimestampRepr::new(TSVAL, 0)));
  }

  #[test]
  fn a_syn_shrinks_when_the_new_block_is_shorter() {
    let input = smoltcp_syn(v4(2), v4(9), MSS_V4, true);
    let out = shaped(OsStack::Windows, input.clone());
    assert_eq!(out.len(), input.len() - 8);
    assert_wire_valid(&out);
  }

  #[test]
  fn a_timestamp_os_without_a_smoltcp_timestamp_still_sends_one() {
    let out = shaped(OsStack::Linux, smoltcp_syn(v4(2), v4(9), MSS_V4, false));
    let parsed = assert_wire_valid(&out);
    assert!(parsed.timestamp.is_some());
    assert_eq!(parsed.timestamp.map(|ts| ts.tsecr), Some(0));
  }

  #[test]
  fn ipv6_syns_get_the_same_blocks_with_the_ipv6_mss_and_hop_limit() {
    for (os, expect_hop, expect_options) in [
      (
        OsStack::Windows,
        128u8,
        OsStack::Windows.syn_options(MSS_V6, 0),
      ),
      (
        OsStack::MacOs,
        64,
        OsStack::MacOs.syn_options(MSS_V6, TSVAL),
      ),
      (
        OsStack::Linux,
        64,
        OsStack::Linux.syn_options(MSS_V6, TSVAL),
      ),
    ] {
      let out = shaped(os, smoltcp_syn(v6(2), v6(9), MSS_V6, true));
      assert_eq!(out[7], expect_hop, "{os:?} hop limit");
      assert_eq!(options(&out), expect_options.as_slice(), "{os:?}");
      assert_eq!(window(&out), os.syn_window());
      let parsed = assert_wire_valid(&out);
      assert_eq!(parsed.max_seg_size, Some(MSS_V6));
      assert_eq!(parsed.window_scale, Some(os.window_shift()));
    }
  }

  #[test]
  fn a_syn_keeps_its_payload_and_addresses() {
    let mut repr = segment(TcpControl::Syn, false, 65535, b"early data");
    repr.max_seg_size = Some(MSS_V4);
    let out = shaped(OsStack::MacOs, emit(v4(2), v4(9), &repr, 64));
    assert!(out.ends_with(b"early data"));
    let parsed = assert_wire_valid(&out);
    assert_eq!(parsed.payload, b"early data");
    assert_eq!(&out[12..16], &[10, 0, 0, 2]);
    assert_eq!(&out[16..20], &[10, 0, 0, 9]);
  }

  #[test]
  fn a_syn_without_an_mss_is_left_alone_apart_from_the_ip_fields() {
    let mut repr = segment(TcpControl::Syn, false, 65535, &[]);
    repr.max_seg_size = None;
    let input = emit(v4(2), v4(9), &repr, 64);
    let out = shaped(OsStack::Windows, input.clone());
    assert_eq!(options(&out), options(&input));
    assert_eq!(window(&out), 65535);
    assert_eq!(out[8], 128);
    assert_wire_valid(&out);
  }

  #[test]
  fn every_packet_gets_the_os_ttl_df_and_an_ip_id() {
    let udp_v4 = |ttl: u8| -> Vec<u8> {
      let mut p = vec![0u8; 20 + 8 + 4];
      p[0] = 0x45;
      p[2..4].copy_from_slice(&32u16.to_be_bytes());
      p[8] = ttl;
      p[9] = 17;
      p[12..16].copy_from_slice(&[10, 0, 0, 2]);
      p[16..20].copy_from_slice(&[1, 1, 1, 1]);
      p[20..22].copy_from_slice(&5000u16.to_be_bytes());
      p[22..24].copy_from_slice(&53u16.to_be_bytes());
      p[24..26].copy_from_slice(&12u16.to_be_bytes());
      p[28..32].copy_from_slice(b"ping");
      p
    };
    for os in [OsStack::Windows, OsStack::MacOs, OsStack::Linux] {
      let input = udp_v4(64);
      let out = shaped(os, input.clone());
      assert_eq!(out.len(), input.len());
      assert_eq!(out[8], os.ttl());
      assert_ne!(out[6] & 0x40, 0);
      assert_ne!(be16(&out[4..6]), 0);
      assert_eq!(&out[20..], &input[20..], "UDP part untouched");
      assert!(Ipv4Packet::new_checked(&out[..]).unwrap().verify_checksum());
    }

    let mut udp_v6 = vec![0u8; 40 + 8];
    udp_v6[0] = 0x60;
    udp_v6[4..6].copy_from_slice(&8u16.to_be_bytes());
    udp_v6[6] = 17;
    udp_v6[7] = 64;
    let out = shaped(OsStack::Windows, udp_v6.clone());
    assert_eq!(out[7], 128);
    assert_eq!(&out[..7], &udp_v6[..7]);
    assert_eq!(&out[8..], &udp_v6[8..]);
  }

  #[test]
  fn an_id_a_sender_already_chose_is_kept_and_ours_never_repeats_or_hits_zero() {
    let mut shaper = PacketShaper::new(OsStack::Linux, u16::MAX);
    let mut ids = Vec::new();
    for _ in 0..3 {
      let mut p = smoltcp_syn(v4(2), v4(9), MSS_V4, false);
      shaper.shape_outbound(&mut p, Instant::now());
      ids.push(be16(&p[4..6]));
    }
    assert_eq!(ids, vec![u16::MAX, 1, 2], "wraps past zero");

    let mut p = smoltcp_syn(v4(2), v4(9), MSS_V4, false);
    p[4..6].copy_from_slice(&0xbeefu16.to_be_bytes());
    shaper.shape_outbound(&mut p, Instant::now());
    assert_eq!(be16(&p[4..6]), 0xbeef);
    assert_wire_valid(&p);
  }

  #[test]
  fn fragments_get_no_df_and_no_tcp_rewrite() {
    let mut input = smoltcp_syn(v4(2), v4(9), MSS_V4, false);
    input[6] = 0x20; // more fragments
    let out = shaped(OsStack::Windows, input.clone());
    assert_eq!(out[6] & 0x40, 0, "no DF on a fragment");
    assert_eq!(out[8], 128);
    assert_eq!(&out[20..], &input[20..], "payload untouched");
  }

  #[test]
  fn window_scaling_never_rounds_a_usable_window_to_zero() {
    for shift in [6u8, 7, 8] {
      for real in 0..=u16::MAX {
        let field = scale_window(real, shift);
        assert_eq!(u32::from(field), u32::from(real) >> shift);
        if field == 0 {
          assert!(
            u32::from(real) < (1u32 << shift),
            "real={real} shift={shift}"
          );
        }
        assert!(
          u32::from(field) << shift <= u32::from(real),
          "never overstates"
        );
      }
    }
    assert_eq!(scale_window(65535, 8), 255);
    assert_eq!(scale_window(65535, 6), 1023);
    assert_eq!(scale_window(65535, 7), 511);
    assert_eq!(scale_window(256, 8), 1);
    assert_eq!(scale_window(255, 8), 0);
    assert_eq!(scale_window(100, 0), 100);
  }

  #[test]
  fn later_segments_are_scaled_once_the_peer_agreed() {
    for (os, v6_ip) in [
      (OsStack::Windows, false),
      (OsStack::MacOs, false),
      (OsStack::Linux, true),
    ] {
      let (local, remote) = if v6_ip {
        (v6(2), v6(9))
      } else {
        (v4(2), v4(9))
      };
      let mut shaper = PacketShaper::new(os, 7);
      let now = Instant::now();
      let mut syn = smoltcp_syn(local, remote, MSS_V4, true);
      shaper.shape_outbound(&mut syn, now);
      shaper.observe_inbound(&peer_syn_ack(local, remote, Some(9)), now);
      assert_eq!(shaper.tracked_flows(), 1);

      let mut ack = emit(
        local,
        remote,
        &segment(TcpControl::None, true, 65535, &[]),
        64,
      );
      shaper.shape_outbound(&mut ack, now);
      assert_eq!(window(&ack), 65535 >> os.window_shift(), "{os:?}");
      let parsed = assert_wire_valid(&ack);
      assert_eq!(parsed.window_len, 65535 >> os.window_shift());

      let mut data = emit(
        local,
        remote,
        &segment(TcpControl::Psh, true, 12345, b"hello world"),
        64,
      );
      shaper.shape_outbound(&mut data, now);
      assert_eq!(window(&data), 12345 >> os.window_shift());
      assert_eq!(assert_wire_valid(&data).payload, b"hello world");
    }
  }

  #[test]
  fn an_unscaled_connection_keeps_its_window() {
    let mut shaper = PacketShaper::new(OsStack::Windows, 7);
    let now = Instant::now();
    let mut syn = smoltcp_syn(v4(2), v4(9), MSS_V4, false);
    shaper.shape_outbound(&mut syn, now);
    shaper.observe_inbound(&peer_syn_ack(v4(2), v4(9), None), now);
    assert_eq!(shaper.tracked_flows(), 0);

    let input = emit(
      v4(2),
      v4(9),
      &segment(TcpControl::None, true, 40000, &[]),
      64,
    );
    let mut ack = input.clone();
    shaper.shape_outbound(&mut ack, now);
    assert_eq!(window(&ack), 40000);
    assert_eq!(ack[8], 128, "the TTL is still stamped");
    assert_wire_valid(&ack);
    assert_eq!(&ack[20..], &input[20..]);
  }

  #[test]
  fn negotiation_is_tracked_per_local_port_and_remote_endpoint() {
    let mut shaper = PacketShaper::new(OsStack::Windows, 7);
    let now = Instant::now();
    shaper.observe_inbound(&peer_syn_ack(v4(2), v4(9), Some(7)), now);

    let ack_to = |remote: IpAddress, remote_port: u16, local_port: u16| {
      let mut repr = segment(TcpControl::None, true, 60000, &[]);
      repr.src_port = local_port;
      repr.dst_port = remote_port;
      emit(v4(2), remote, &repr, 64)
    };
    let mut other_port = ack_to(v4(9), 443, 40001);
    let mut other_remote = ack_to(v4(8), 443, 40000);
    let mut other_remote_port = ack_to(v4(9), 8443, 40000);
    let mut same = ack_to(v4(9), 443, 40000);
    for p in [
      &mut other_port,
      &mut other_remote,
      &mut other_remote_port,
      &mut same,
    ] {
      shaper.shape_outbound(p, now);
    }
    assert_eq!(window(&other_port), 60000);
    assert_eq!(window(&other_remote), 60000);
    assert_eq!(window(&other_remote_port), 60000);
    assert_eq!(window(&same), 60000 >> 8);
  }

  #[test]
  fn a_new_syn_on_the_same_tuple_forgets_the_old_negotiation() {
    let mut shaper = PacketShaper::new(OsStack::Linux, 7);
    let now = Instant::now();
    shaper.observe_inbound(&peer_syn_ack(v4(2), v4(9), Some(7)), now);
    assert_eq!(shaper.tracked_flows(), 1);
    let mut syn = smoltcp_syn(v4(2), v4(9), MSS_V4, true);
    shaper.shape_outbound(&mut syn, now);
    assert_eq!(shaper.tracked_flows(), 0);
  }

  #[test]
  fn a_reset_in_either_direction_ends_tracking() {
    let now = Instant::now();
    for outbound in [false, true] {
      let mut shaper = PacketShaper::new(OsStack::Linux, 7);
      shaper.observe_inbound(&peer_syn_ack(v4(2), v4(9), Some(7)), now);
      assert_eq!(shaper.tracked_flows(), 1);
      if outbound {
        let mut rst = emit(v4(2), v4(9), &segment(TcpControl::Rst, true, 0, &[]), 64);
        shaper.shape_outbound(&mut rst, now);
      } else {
        let mut repr = segment(TcpControl::Rst, true, 0, &[]);
        repr.src_port = 443;
        repr.dst_port = 40000;
        shaper.observe_inbound(&emit(v4(9), v4(2), &repr, 56), now);
      }
      assert_eq!(shaper.tracked_flows(), 0, "outbound={outbound}");
    }
  }

  #[test]
  fn a_connection_closed_by_both_sides_lingers_then_goes() {
    let mut shaper = PacketShaper::new(OsStack::Linux, 7);
    let now = Instant::now();
    shaper.observe_inbound(&peer_syn_ack(v4(2), v4(9), Some(7)), now);

    let mut fin = emit(
      v4(2),
      v4(9),
      &segment(TcpControl::Fin, true, 65535, &[]),
      64,
    );
    shaper.shape_outbound(&mut fin, now);
    shaper.sweep(now + FLOW_CLOSE_LINGER * 2);
    assert_eq!(shaper.tracked_flows(), 1, "one FIN is not a close");

    let mut repr = segment(TcpControl::Fin, true, 29200, &[]);
    repr.src_port = 443;
    repr.dst_port = 40000;
    let peer_fin = emit(v4(9), v4(2), &repr, 56);
    shaper.observe_inbound(&peer_fin, now);
    shaper.sweep(now + FLOW_CLOSE_LINGER / 2);
    assert_eq!(shaper.tracked_flows(), 1, "still inside the linger");

    let mut last_ack = emit(
      v4(2),
      v4(9),
      &segment(TcpControl::None, true, 65535, &[]),
      64,
    );
    shaper.shape_outbound(&mut last_ack, now + FLOW_CLOSE_LINGER / 2);
    assert_eq!(
      window(&last_ack),
      65535 >> 7,
      "the closing ACK is still scaled"
    );

    shaper.sweep(now + FLOW_CLOSE_LINGER);
    assert_eq!(shaper.tracked_flows(), 0);
  }

  #[test]
  fn an_idle_connection_ages_out_and_a_busy_one_does_not() {
    let mut shaper = PacketShaper::new(OsStack::Linux, 7);
    let now = Instant::now();
    shaper.observe_inbound(&peer_syn_ack(v4(2), v4(9), Some(7)), now);
    let mut busy = emit(
      v4(2),
      v4(9),
      &segment(TcpControl::None, true, 1000, &[]),
      64,
    );
    shaper.shape_outbound(&mut busy, now + FLOW_IDLE_TTL / 2);
    shaper.sweep(now + FLOW_IDLE_TTL);
    assert_eq!(shaper.tracked_flows(), 1, "traffic refreshed it");
    shaper.sweep(now + FLOW_IDLE_TTL + FLOW_IDLE_TTL / 2);
    assert_eq!(shaper.tracked_flows(), 0);
  }

  #[test]
  fn the_flow_table_is_bounded() {
    let mut shaper = PacketShaper::new(OsStack::Linux, 7);
    let now = Instant::now();
    let template = peer_syn_ack(v4(2), v4(9), Some(7));
    let l4 = header_len(&template);
    for i in 0..(MAX_FLOWS + 50) {
      let mut p = template.clone();
      p[l4 + 2..l4 + 4].copy_from_slice(&(10000 + (i % 50000) as u16).to_be_bytes());
      p[l4..l4 + 2].copy_from_slice(&(1 + (i / 50000) as u16).to_be_bytes());
      p[12..16].copy_from_slice(&[10, (i >> 16) as u8, (i >> 8) as u8, i as u8]);
      shaper.observe_inbound(&p, now + Duration::from_millis(i as u64));
      assert!(shaper.tracked_flows() <= MAX_FLOWS);
    }
    assert_eq!(shaper.tracked_flows(), MAX_FLOWS);
  }

  #[test]
  fn truncated_and_malformed_packets_pass_through_without_panicking() {
    let full = smoltcp_syn(v4(2), v4(9), MSS_V4, true);
    let full6 = smoltcp_syn(v6(2), v6(9), MSS_V6, true);
    for source in [&full, &full6] {
      for len in 0..source.len() {
        let mut p = source[..len].to_vec();
        let mut shaper = PacketShaper::new(OsStack::MacOs, 7);
        shaper.shape_outbound(&mut p, Instant::now());
        shaper.observe_inbound(&source[..len], Instant::now());
        assert_eq!(p, source[..len], "len={len} is left untouched");
      }
    }
    let mut bad_offset = full.clone();
    bad_offset[20 + 12] = 0xf0; // data offset past the datagram
    let before = bad_offset.clone();
    let mut shaper = PacketShaper::new(OsStack::Windows, 7);
    shaper.shape_outbound(&mut bad_offset, Instant::now());
    assert_eq!(window(&bad_offset), window(&before));
    assert_eq!(options(&full).len(), 20);
  }

  #[test]
  fn option_parsing_skips_nops_stops_at_eol_and_rejects_bad_lengths() {
    let parsed = parse_tcp_options(&[
      1, 1, 2, 4, 5, 100, 1, 3, 3, 9, 8, 10, 0, 0, 0, 7, 0, 0, 0, 0,
    ]);
    assert_eq!(
      parsed,
      TcpOptions {
        mss: Some(1380),
        window_scale: Some(9),
        tsval: Some(7)
      }
    );
    assert_eq!(
      parse_tcp_options(&[2, 4, 5, 100, 0, 3, 3, 9]).window_scale,
      None
    );
    assert_eq!(parse_tcp_options(&[2, 9, 5]), TcpOptions::default());
    assert_eq!(parse_tcp_options(&[3, 1, 0]), TcpOptions::default());
    assert_eq!(parse_tcp_options(&[2]), TcpOptions::default());
  }

  #[test]
  fn checksums_match_an_independent_computation_for_odd_lengths() {
    let mut repr = segment(TcpControl::Psh, true, 65535, b"odd");
    repr.max_seg_size = None;
    let mut shaper = PacketShaper::new(OsStack::Windows, 7);
    let now = Instant::now();
    shaper.observe_inbound(&peer_syn_ack(v4(2), v4(9), Some(8)), now);
    let mut p = emit(v4(2), v4(9), &repr, 64);
    shaper.shape_outbound(&mut p, now);
    assert_wire_valid(&p);
  }

  #[test]
  fn the_timestamp_clock_only_moves_forward() {
    let first = tsval_now();
    std::thread::sleep(Duration::from_millis(5));
    let second = tsval_now();
    let elapsed = second.wrapping_sub(first);
    assert!((5..5000).contains(&elapsed), "elapsed={elapsed}");
  }
}
