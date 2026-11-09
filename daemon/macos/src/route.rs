//! Picking a wired path to Windows over Wi-Fi.
//!
//! `windows-pc.local` resolves over mDNS to one IPv6 link-local address per
//! interface that can see Windows (plus routed IPv4/IPv6 ones), and a TCP
//! connection stays on whichever interface it started on. Live-found
//! 2026-11-09: with an ethernet cable plugged in, the bridge sat on Wi-Fi
//! (`fe80::…%en0`, 50 ms average / 135 ms worst round trip, input arriving
//! in bursts) while the same Windows box answered over the cable in under
//! 1 ms. So addresses are tried wired-first, and a connection that ended up
//! on Wi-Fi moves over once a wired path appears.

use std::ffi::CStr;
use std::io;
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::time::Duration;

/// How long one candidate address gets before the next is tried, so a
/// stale wired address can't hold up falling back to Wi-Fi.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// `_IOWR('i', 173, struct ifreq)` on macOS; not exported by libc. The same
/// query `ifconfig -v` prints as "type: Wi-Fi". (The media type is no use
/// here: Apple's Wi-Fi reports itself as Ethernet media.)
const SIOCGIFFUNCTIONALTYPE: libc::c_ulong = 0xc020_69ad;
const IFRTYPE_FUNCTIONAL_WIFI_INFRA: u32 = 3;
const IFRTYPE_FUNCTIONAL_WIFI_AWDL: u32 = 4;

#[repr(C)]
struct IfReqFunctionalType {
    ifr_name: [libc::c_char; libc::IFNAMSIZ],
    ifr_functional_type: u32,
    _pad: [u8; 12],
}

/// Whether `ifname` is a Wi-Fi interface, whatever enN number it has.
pub fn is_wifi(ifname: &str) -> bool {
    let mut req: IfReqFunctionalType = unsafe { std::mem::zeroed() };
    for (dst, src) in req.ifr_name.iter_mut().zip(ifname.bytes().take(libc::IFNAMSIZ - 1)) {
        *dst = src as libc::c_char;
    }
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        if fd < 0 {
            return false;
        }
        let ok = libc::ioctl(fd, SIOCGIFFUNCTIONALTYPE, &mut req) == 0;
        libc::close(fd);
        ok && matches!(req.ifr_functional_type, IFRTYPE_FUNCTIONAL_WIFI_INFRA | IFRTYPE_FUNCTIONAL_WIFI_AWDL)
    }
}

fn index_to_name(index: u32) -> Option<String> {
    let mut buf = [0 as libc::c_char; libc::IFNAMSIZ];
    let p = unsafe { libc::if_indextoname(index, buf.as_mut_ptr()) };
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

/// The interface that owns local address `ip`.
fn owner_of(ip: IpAddr) -> Option<String> {
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return None;
    }
    let mut found = None;
    let mut cur = head;
    while !cur.is_null() {
        let ifa = unsafe { &*cur };
        cur = ifa.ifa_next;
        if ifa.ifa_addr.is_null() {
            continue;
        }
        let addr = unsafe {
            match (*ifa.ifa_addr).sa_family as libc::c_int {
                libc::AF_INET => {
                    let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                    IpAddr::from(u32::from_be(sin.sin_addr.s_addr).to_be_bytes())
                }
                libc::AF_INET6 => {
                    let sin6 = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                    IpAddr::from(sin6.sin6_addr.s6_addr)
                }
                _ => continue,
            }
        };
        if addr == ip {
            found = Some(unsafe { CStr::from_ptr(ifa.ifa_name) }.to_string_lossy().into_owned());
            break;
        }
    }
    unsafe { libc::freeifaddrs(head) };
    found
}

/// The interface a socket bound to local address `local` goes out on.
pub fn interface_of_local(local: SocketAddr) -> Option<String> {
    match local {
        SocketAddr::V6(v6) if v6.scope_id() != 0 => index_to_name(v6.scope_id()),
        _ => owner_of(local.ip()),
    }
}

/// The interface traffic to `peer` would leave on. A link-local address
/// names it in its scope; anything else is asked of the routing table with
/// an unsent UDP "connect".
pub fn interface_to(peer: SocketAddr) -> Option<String> {
    if let SocketAddr::V6(v6) = peer
        && v6.scope_id() != 0
    {
        return index_to_name(v6.scope_id());
    }
    let bind: SocketAddr = if peer.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }.parse().unwrap();
    let probe = UdpSocket::bind(bind).ok()?;
    probe.connect(peer).ok()?;
    interface_of_local(probe.local_addr().ok()?)
}

/// `host:port` resolved and ordered wired-first (resolver order otherwise
/// kept), each with the interface it would use.
pub fn ranked_candidates(target: &str) -> io::Result<Vec<(SocketAddr, Option<String>)>> {
    let mut seen = Vec::new();
    for addr in target.to_socket_addrs()? {
        if !seen.iter().any(|(a, _)| *a == addr) {
            seen.push((addr, interface_to(addr)));
        }
    }
    seen.sort_by_key(|(_, iface)| iface.as_deref().is_none_or(is_wifi));
    Ok(seen)
}

/// Connects to `target`, trying wired paths before Wi-Fi.
pub fn connect_preferring_wired(target: &str) -> io::Result<TcpStream> {
    let mut last_err = io::Error::new(io::ErrorKind::NotFound, format!("{target} resolved to no addresses"));
    for (addr, iface) in ranked_candidates(target)? {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(stream) => {
                let via = stream.local_addr().ok().and_then(interface_of_local).or(iface);
                let via = via.map(|i| format!("{i}{}", if is_wifi(&i) { " (Wi-Fi)" } else { "" }));
                println!("Connected to {addr} via {}.", via.as_deref().unwrap_or("unknown interface"));
                return Ok(stream);
            }
            Err(e) => {
                println!("  {addr} ({}) failed: {e}", iface.as_deref().unwrap_or("?"));
                last_err = e;
            }
        }
    }
    Err(last_err)
}

#[cfg(test)]
mod tests {
    #[test]
    fn loopback_is_not_wifi() {
        assert!(!super::is_wifi("lo0"));
    }
}
