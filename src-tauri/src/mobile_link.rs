//! Is this client's route to the host a phone link (USB / Wi-Fi tethering, a cellular modem)?
//!
//! A phone link reads its burst capacity in the 1 s session-start probe (2026-11-15: 21 Mbit
//! read on a USB tether that held ~12–14) and stalls for seconds on a handover; the adaptive
//! controller starts it lower and climbs it more gently (`pulsar_core::adapt::Config::mobile`).
//!
//! Best effort, once per session start, never an error: `Unknown` when it can't tell.
//! - **Windows:** the adapter the route to the peer (or, behind an overlay VPN such as ZeroTier,
//!   the default route) leaves on — `GetBestInterfaceEx` + `GetAdaptersAddresses`: a WWAN
//!   interface type (243 / 244), or a description naming a phone link ("Remote NDIS" — Android
//!   and most phones over USB, "Apple Mobile Device Ethernet" — iPhone USB, "Mobile Broadband").
//! - **macOS:** `route -n get` → the interface, `networksetup -listallhardwareports` → its
//!   hardware port ("iPhone USB", "… Mobile Broadband").
//! - **Linux:** the default route's interface (`/proc/net/route`) → its driver (`rndis_host`,
//!   `ipheth`, `qmi_wwan`, `cdc_mbim`, `huawei_cdc_ncm`) or `DEVTYPE=wwan`.
//! - **Everywhere:** a gateway of `172.20.10.1` is an iPhone Personal Hotspot (USB or Wi-Fi).

use std::net::IpAddr;

/// What the route runs over.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LinkKind {
	/// A phone / cellular link; the string says why (for the log).
	Mobile(String),
	/// A fixed link (Ethernet, Wi-Fi to a router, …) as far as the checks can tell.
	Fixed,
	/// Couldn't tell.
	Unknown,
}

impl LinkKind {
	pub(crate) fn is_mobile(&self) -> bool {
		matches!(self, LinkKind::Mobile(_))
	}
	pub(crate) fn label(&self) -> &'static str {
		match self {
			LinkKind::Mobile(_) => "mobile",
			LinkKind::Fixed => "fixed",
			LinkKind::Unknown => "unknown",
		}
	}
	pub(crate) fn why(&self) -> &str {
		match self {
			LinkKind::Mobile(w) => w,
			_ => "",
		}
	}
}

/// The iPhone Personal Hotspot's gateway (its fixed 172.20.10.0/28 network, USB and Wi-Fi).
#[cfg_attr(target_os = "macos", allow(dead_code))]
const IPHONE_HOTSPOT_GW: [u8; 4] = [172, 20, 10, 1];

/// Adapter descriptions / hardware-port names that mean a phone link (case-insensitive).
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
const MOBILE_HINTS: [&str; 8] = [
	"remote ndis",
	"rndis",
	"apple mobile device ethernet",
	"iphone",
	"ipad",
	"mobile broadband",
	"android",
	"wwan",
];

/// Overlay / VPN adapters: the route to the peer leaves on one of these, the real link is under
/// it — look at the default route instead.
const VIRTUAL_HINTS: [&str; 7] = ["zerotier", "wireguard", "wintun", "tap-windows", "tailscale", "openvpn", "vpn"];

/// A description / port name that means a phone link.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
pub(crate) fn mobile_hint(desc: &str) -> Option<&'static str> {
	let d = desc.to_ascii_lowercase();
	MOBILE_HINTS.into_iter().find(|h| d.contains(h))
}

#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn virtual_adapter(desc: &str) -> bool {
	let d = desc.to_ascii_lowercase();
	VIRTUAL_HINTS.iter().any(|h| d.contains(h))
}

/// Linux: the interface of the `/proc/net/route` entry for `peer` (longest prefix, then the
/// lowest metric; only routes that are up). `Destination` / `Mask` are the address bytes as a
/// host-order hex u32, so `to_le_bytes` gives them back in network order.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn linux_route_dev(table: &str, peer: std::net::Ipv4Addr) -> Option<String> {
	let ip = u32::from_le_bytes(peer.octets());
	let mut best: Option<(u32, u32, String)> = None;
	for l in table.lines().skip(1) {
		let f: Vec<&str> = l.split_whitespace().collect();
		if f.len() < 8 {
			continue;
		}
		let (Ok(dest), Ok(flags), Ok(mask)) =
			(u32::from_str_radix(f[1], 16), u32::from_str_radix(f[3], 16), u32::from_str_radix(f[7], 16))
		else {
			continue;
		};
		if flags & 1 == 0 || ip & mask != dest & mask {
			continue;
		}
		let prefix = mask.count_ones();
		let metric = f[6].parse().unwrap_or(u32::MAX);
		if best.as_ref().is_none_or(|b| prefix > b.0 || (prefix == b.0 && metric < b.1)) {
			best = Some((prefix, metric, f[0].to_string()));
		}
	}
	best.map(|(_, _, dev)| dev)
}

/// Linux overlay / VPN interfaces: the link under them can't be seen, so the default route's
/// device stands in (as `route_adapter` on Windows and `peer_route` on macOS do).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn linux_overlay_dev(dev: &str) -> bool {
	["wg", "tun", "tap", "zt", "tailscale", "ppp"].iter().any(|p| dev.starts_with(p))
}

/// Linux network drivers that are a phone or a cellular modem.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn mobile_driver(driver: &str) -> bool {
	matches!(driver, "rndis_host" | "ipheth" | "qmi_wwan" | "cdc_mbim" | "huawei_cdc_ncm" | "option" | "sierra_net")
}

/// Detect the link kind of the route to `peer` (None / a private overlay address = the default
/// route). Blocking (a few ms; on macOS two short subprocesses) — call off the async runtime.
pub(crate) fn detect(peer: Option<IpAddr>) -> LinkKind {
	let kind = imp::detect(peer);
	tracing::debug!(?kind, ?peer, "route link kind");
	kind
}

/// Does this client's route to `peer` leave on a Wi-Fi adapter? `None` = can't tell. Same
/// route lookup as [`detect`]: Windows `IfType` 71 (IEEE 802.11), macOS a "Wi-Fi" / "AirPort"
/// hardware port, Linux a `wireless` / `phy80211` entry under `/sys/class/net/<dev>`. The
/// "Otomatik" audio pick keeps uncompressed audio off Wi-Fi (hidden-gaps 1.11a: L16 has no
/// concealment worth the name, and Wi-Fi loses and reorders in bursts).
pub(crate) fn wifi(peer: Option<IpAddr>) -> Option<bool> {
	let w = imp::wifi(peer);
	tracing::debug!(?w, ?peer, "route on wi-fi");
	w
}

#[cfg(windows)]
mod imp {
	use super::*;
	use windows_sys::Win32::NetworkManagement::IpHelper::{
		GetAdaptersAddresses, GetBestInterfaceEx, GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST,
		GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH,
	};
	use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_UNSPEC, SOCKADDR, SOCKADDR_IN};

	/// IF_TYPE_WWANPP / IF_TYPE_WWANPP2 (mobile broadband).
	const IF_TYPE_WWAN: [u32; 2] = [243, 244];

	struct Adapter {
		index: u32,
		if_type: u32,
		desc: String,
		name: String,
		gateways: Vec<[u8; 4]>,
	}

	fn wide(p: *const u16) -> String {
		if p.is_null() {
			return String::new();
		}
		let mut n = 0;
		// SAFETY: a NUL-terminated wide string owned by the GetAdaptersAddresses buffer.
		unsafe {
			while *p.add(n) != 0 {
				n += 1;
			}
			String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
		}
	}

	fn adapters() -> Vec<Adapter> {
		let flags = GAA_FLAG_INCLUDE_GATEWAYS | GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
		let mut size: u32 = 32 * 1024;
		for _ in 0..3 {
			// u64 storage keeps the buffer aligned for IP_ADAPTER_ADDRESSES_LH.
			let mut buf = vec![0u64; (size as usize).div_ceil(8)];
			let head = buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH;
			// SAFETY: `buf` holds `size` bytes; the API fills it with a linked list inside it.
			let rc = unsafe { GetAdaptersAddresses(AF_UNSPEC as u32, flags, std::ptr::null(), head, &mut size) };
			if rc == 111 {
				// ERROR_BUFFER_OVERFLOW: `size` now holds what it needs.
				continue;
			}
			if rc != 0 {
				return Vec::new();
			}
			let mut out = Vec::new();
			let mut cur = head as *const IP_ADAPTER_ADDRESSES_LH;
			while !cur.is_null() {
				// SAFETY: a node of the list the API built inside `buf` (alive in this scope).
				let a = unsafe { &*cur };
				let mut gateways = Vec::new();
				let mut g = a.FirstGatewayAddress;
				while !g.is_null() {
					// SAFETY: list nodes inside `buf`; the sockaddr pointer comes from the same list.
					let ga = unsafe { &*g };
					let sa = ga.Address.lpSockaddr;
					if !sa.is_null() && unsafe { (*sa).sa_family } == AF_INET {
						let sin = unsafe { &*(sa as *const SOCKADDR_IN) };
						let ip = unsafe { sin.sin_addr.S_un.S_addr }.to_ne_bytes();
						gateways.push(ip);
					}
					g = ga.Next;
				}
				out.push(Adapter {
					// SAFETY: the union's struct view (Length, IfIndex) is what the API fills.
					index: unsafe { a.Anonymous1.Anonymous.IfIndex },
					if_type: a.IfType,
					desc: wide(a.Description),
					name: wide(a.FriendlyName),
					gateways,
				});
				cur = a.Next;
			}
			return out;
		}
		Vec::new()
	}

	fn best_if(ip: [u8; 4]) -> Option<u32> {
		let mut sin: SOCKADDR_IN = unsafe { std::mem::zeroed() };
		sin.sin_family = AF_INET;
		sin.sin_addr.S_un.S_addr = u32::from_ne_bytes(ip);
		let mut idx = 0u32;
		// SAFETY: a valid IPv4 sockaddr; GetBestInterfaceEx only reads it (no packet is sent).
		let rc = unsafe { GetBestInterfaceEx(&sin as *const SOCKADDR_IN as *const SOCKADDR, &mut idx) };
		(rc == 0).then_some(idx)
	}

	fn judge(a: &Adapter) -> LinkKind {
		if IF_TYPE_WWAN.contains(&a.if_type) {
			return LinkKind::Mobile(format!("WWAN adapter \"{}\"", a.desc));
		}
		if let Some(h) = mobile_hint(&a.desc).or_else(|| mobile_hint(&a.name)) {
			return LinkKind::Mobile(format!("{h}: \"{}\"", a.desc));
		}
		if a.gateways.contains(&IPHONE_HOTSPOT_GW) {
			return LinkKind::Mobile("iPhone hotspot gateway 172.20.10.1".into());
		}
		LinkKind::Fixed
	}

	/// The adapter the route to `peer` leaves on, unless that is an overlay adapter (then the
	/// default route's).
	fn route_adapter(list: &[Adapter], peer: Option<IpAddr>) -> Option<&Adapter> {
		let find = |ip: [u8; 4]| best_if(ip).and_then(|i| list.iter().find(|a| a.index == i));
		let peer_v4 = match peer {
			Some(IpAddr::V4(v4)) if !v4.is_loopback() && !v4.is_unspecified() => Some(v4.octets()),
			_ => None,
		};
		let mut adapter = peer_v4.and_then(find);
		if adapter.is_none_or(|a| virtual_adapter(&a.desc) || virtual_adapter(&a.name)) {
			adapter = find([1, 1, 1, 1]);
		}
		adapter
	}

	pub(super) fn detect(peer: Option<IpAddr>) -> LinkKind {
		let list = adapters();
		if list.is_empty() {
			return LinkKind::Unknown;
		}
		route_adapter(&list, peer).map_or(LinkKind::Unknown, judge)
	}

	/// IF_TYPE_IEEE80211.
	const IF_TYPE_WIFI: u32 = 71;

	pub(super) fn wifi(peer: Option<IpAddr>) -> Option<bool> {
		let list = adapters();
		route_adapter(&list, peer).map(|a| a.if_type == IF_TYPE_WIFI)
	}
}

#[cfg(target_os = "macos")]
mod imp {
	use super::*;

	fn run(exe: &str, args: &[&str]) -> Option<String> {
		let out = std::process::Command::new(exe).args(args).output().ok()?;
		out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
	}

	/// `route -n get <dest>` → (interface, gateway).
	fn route(dest: &str) -> Option<(String, String)> {
		let txt = run("/sbin/route", &["-n", "get", dest])?;
		let field = |k: &str| {
			txt.lines().find_map(|l| l.trim().strip_prefix(k).map(|v| v.trim_start_matches(':').trim().to_string()))
		};
		Some((field("interface")?, field("gateway").unwrap_or_default()))
	}

	/// The hardware port name of `dev` ("iPhone USB", "Wi-Fi", …).
	fn hardware_port(dev: &str) -> Option<String> {
		let txt = run("/usr/sbin/networksetup", &["-listallhardwareports"])?;
		let mut port = None;
		for l in txt.lines() {
			if let Some(p) = l.strip_prefix("Hardware Port:") {
				port = Some(p.trim().to_string());
			} else if let Some(d) = l.strip_prefix("Device:") {
				if d.trim() == dev {
					return port;
				}
			}
		}
		None
	}

	/// (interface, gateway) of the route to `peer`, or the default route's behind an overlay.
	fn peer_route(peer: Option<IpAddr>) -> Option<(String, String)> {
		let dest = peer.filter(|p| !p.is_loopback()).map(|p| p.to_string());
		let mut r = dest.as_deref().and_then(route);
		if r.as_ref().is_none_or(|(dev, _)| dev.starts_with("utun") || dev.starts_with("feth") || dev.starts_with("zt")) {
			r = route("default");
		}
		r
	}

	pub(super) fn wifi(peer: Option<IpAddr>) -> Option<bool> {
		let (dev, _) = peer_route(peer)?;
		let port = hardware_port(&dev)?;
		Some(port.contains("Wi-Fi") || port.contains("AirPort"))
	}

	pub(super) fn detect(peer: Option<IpAddr>) -> LinkKind {
		let Some((dev, gw)) = peer_route(peer) else { return LinkKind::Unknown };
		if gw == "172.20.10.1" {
			return LinkKind::Mobile("iPhone hotspot gateway 172.20.10.1".into());
		}
		match hardware_port(&dev) {
			Some(port) => match mobile_hint(&port) {
				Some(h) => LinkKind::Mobile(format!("{h}: \"{port}\" ({dev})")),
				None if virtual_adapter(&port) => LinkKind::Unknown,
				None => LinkKind::Fixed,
			},
			None => LinkKind::Unknown,
		}
	}
}

#[cfg(target_os = "linux")]
mod imp {
	use super::*;

	/// The default route's interface and gateway from `/proc/net/route` (lowest metric).
	pub(super) fn default_route(table: &str) -> Option<(String, [u8; 4])> {
		let mut best: Option<(u32, String, [u8; 4])> = None;
		for l in table.lines().skip(1) {
			let f: Vec<&str> = l.split_whitespace().collect();
			if f.len() < 7 || f[1] != "00000000" {
				continue;
			}
			let gw = u32::from_str_radix(f[2], 16).ok()?.to_le_bytes();
			let metric = f[6].parse().unwrap_or(u32::MAX);
			if best.as_ref().is_none_or(|b| metric < b.0) {
				best = Some((metric, f[0].to_string(), gw));
			}
		}
		best.map(|(_, i, g)| (i, g))
	}

	/// Whether the route to `peer` leaves on a Wi-Fi interface: the peer's own route (a LAN
	/// host can sit behind wlan0 while the default route is a wired NIC or a VPN), or the
	/// default route's when there is no IPv4 peer or its route is an overlay.
	pub(super) fn wifi(peer: Option<IpAddr>) -> Option<bool> {
		let table = std::fs::read_to_string("/proc/net/route").ok()?;
		let peer_v4 = match peer {
			Some(IpAddr::V4(v4)) if !v4.is_loopback() && !v4.is_unspecified() => Some(v4),
			_ => None,
		};
		let mut dev = peer_v4.and_then(|p| super::linux_route_dev(&table, p));
		if dev.as_deref().is_none_or(super::linux_overlay_dev) {
			dev = default_route(&table).map(|(d, _)| d);
		}
		let dev = dev?;
		let sys = std::path::Path::new("/sys/class/net").join(&dev);
		Some(sys.join("wireless").exists() || sys.join("phy80211").exists())
	}

	pub(super) fn detect(_peer: Option<IpAddr>) -> LinkKind {
		let Some((dev, gw)) = std::fs::read_to_string("/proc/net/route").ok().as_deref().and_then(default_route) else {
			return LinkKind::Unknown;
		};
		if gw == IPHONE_HOTSPOT_GW {
			return LinkKind::Mobile("iPhone hotspot gateway 172.20.10.1".into());
		}
		let sys = format!("/sys/class/net/{dev}");
		let driver = std::fs::read_link(format!("{sys}/device/driver"))
			.ok()
			.and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
			.unwrap_or_default();
		if mobile_driver(&driver) {
			return LinkKind::Mobile(format!("driver {driver} ({dev})"));
		}
		let uevent = std::fs::read_to_string(format!("{sys}/uevent")).unwrap_or_default();
		if uevent.lines().any(|l| l.trim() == "DEVTYPE=wwan") || dev.starts_with("wwan") {
			return LinkKind::Mobile(format!("wwan interface {dev}"));
		}
		if driver.is_empty() {
			// A virtual interface (VPN, bridge): can't see the link under it.
			return LinkKind::Unknown;
		}
		LinkKind::Fixed
	}
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
mod imp {
	use super::*;
	pub(super) fn detect(_peer: Option<IpAddr>) -> LinkKind {
		LinkKind::Unknown
	}
	pub(super) fn wifi(_peer: Option<IpAddr>) -> Option<bool> {
		None
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn phone_adapters_are_recognised_by_name() {
		assert_eq!(mobile_hint("Remote NDIS Compatible Device"), Some("remote ndis"));
		assert!(mobile_hint("Apple Mobile Device Ethernet").is_some());
		assert!(mobile_hint("iPhone USB").is_some());
		assert!(mobile_hint("Generic Mobile Broadband Adapter").is_some());
		assert!(mobile_hint("Qualcomm FastConnect C7700 Wi-Fi 7 Network Adapter").is_none());
		assert!(mobile_hint("Realtek USB GbE Family Controller").is_none());
		assert!(virtual_adapter("ZeroTier Virtual Port"));
		assert!(!virtual_adapter("Intel(R) Ethernet Connection"));
	}

	#[test]
	fn linux_phone_drivers() {
		assert!(mobile_driver("rndis_host"));
		assert!(mobile_driver("ipheth"));
		assert!(mobile_driver("qmi_wwan"));
		assert!(!mobile_driver("r8169"));
		assert!(!mobile_driver("iwlwifi"));
	}

	#[cfg(target_os = "linux")]
	#[test]
	fn linux_default_route_is_parsed() {
		let t = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\n\
		         wlan0\t00000000\t0144A8C0\t0003\t0\t0\t600\t00000000\n\
		         usb0\t00000000\t010A14AC\t0003\t0\t0\t100\t00000000\n";
		assert_eq!(imp::default_route(t), Some(("usb0".to_string(), [172, 20, 10, 1])));
	}

	#[test]
	fn linux_peer_route_takes_the_longest_prefix() {
		// Default via eth0 (metric 100), 192.168.68.0/24 on wlan0, a ZeroTier /24 and a down
		// route; Destination / Mask as /proc/net/route prints them (host-order hex).
		let t = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT\n\
		         eth0\t00000000\t0100A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
		         wlan0\t0044A8C0\t00000000\t0001\t0\t0\t600\t00FFFFFF\t0\t0\t0\n\
		         ztabc\t000F790A\t00000000\t0001\t0\t0\t5000\t00FFFFFF\t0\t0\t0\n\
		         eth1\t3D44A8C0\t00000000\t0000\t0\t0\t0\tFFFFFFFF\t0\t0\t0\n";
		let dev = |ip: [u8; 4]| linux_route_dev(t, ip.into());
		assert_eq!(dev([192, 168, 68, 61]).as_deref(), Some("wlan0"), "the down /32 is skipped");
		assert_eq!(dev([10, 121, 15, 149]).as_deref(), Some("ztabc"));
		assert_eq!(dev([1, 1, 1, 1]).as_deref(), Some("eth0"));
		assert!(linux_overlay_dev("ztabc") && linux_overlay_dev("wg0") && !linux_overlay_dev("wlan0"));
	}

	/// Runs on the machine itself: never panics, always answers (manual check of the live route).
	#[test]
	fn detect_answers_on_this_machine() {
		let k = detect(None);
		println!("this machine's default route: {} {}", k.label(), k.why());
	}
}
