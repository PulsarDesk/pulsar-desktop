//! Minimal RTP/H.264 packetizer (RFC 6184) — byte-for-byte compatible with the
//! Pulsar client depacketizer (`desktop-app/src/lib/h264.ts`) and ffmpeg's RTP
//! demuxer (the Pi native client). This is the pure-Rust replacement for the
//! ffmpeg `-f rtp -payload_type 96` muxer used on the NVENC fast path; emitting
//! it ourselves lets the native capture pipeline skip libavformat for transport.
//!
//! Wire contract (exactly what the client parses — see `h264.ts::push`):
//!
//! * **RTP header** — 12 bytes, no CSRCs, no extension, no padding:
//!   ```text
//!    0                   1                   2                   3
//!    0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
//!   +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//!   |V=2|P|X|  CC   |M|     PT=96   |       sequence number         |
//!   +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//!   |                           timestamp (90 kHz)                  |
//!   +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//!   |                            SSRC (random)                      |
//!   +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//!   ```
//!   byte0 = `0x80` (V=2, P=0, X=0, CC=0); byte1 = `M<<7 | 96`. Sequence,
//!   timestamp and SSRC are big-endian. The client masks X (`0x10`) and CC
//!   (`0x0f`) out of byte0, M (`0x80`) out of byte1, so we keep both zero.
//!
//! * **Single-NAL packet** (NAL ≤ MTU): the payload IS the NAL (header byte
//!   `[F|NRI|type]` followed by the RBSP). Client: `t = pl[0] & 0x1f` in 1..=23.
//!
//! * **FU-A** (NAL > MTU, type 28): each fragment is
//!   `[FU indicator][FU header][fragment bytes]` where
//!   - FU indicator = `(orig_hdr & 0xE0) | 28` — carries the original F+NRI bits.
//!   - FU header    = `(S<<7) | (E<<6) | (orig_hdr & 0x1f)` — S on the first
//!     fragment, E on the last, original NAL type in the low 5 bits.
//!   The client rebuilds the NAL header as `(pl[0] & 0x60) | (pl[1] & 0x1f)`, so
//!   the NRI MUST live in the indicator's top bits (it does) — see `h264.ts`.
//!
//! * **Marker bit** is set on the LAST RTP packet of an access unit (the client
//!   calls `emitAU()` only when M=1). Sequence number increments by one per
//!   packet (wraps at 2^16); the 90 kHz timestamp is constant across every
//!   packet of one access unit. PT=96, 90 kHz, SSRC random per sender — matching
//!   the ffmpeg muxer the client was verified against.
//!
//! SPS/PPS are emitted in-band as their own single-NAL packets ahead of the IDR
//! (the host's `dump_extra` already prepends them to the Annex-B keyframe), so
//! this packetizer needs no special parameter-set handling — they fall out of
//! the generic NAL split below.

use crate::Codec;
use rand::RngExt;
use std::collections::VecDeque;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Access units recently put on the wire: `(rtp_ts, first_seq, last_seq)`, oldest first.
/// Reference-frame invalidation maps a client's lost sequence range back to its frames.
pub type SentLog = Arc<Mutex<VecDeque<(u32, u16, u16)>>>;
/// Entries kept (~2 s at 120 fps — far beyond any useful invalidation depth).
const SENT_LOG_LEN: usize = 256;

/// RTP timestamps of the logged access units whose packets overlap `first..=last`
/// (sequence numbers wrap).
pub fn frames_in_seq_range(log: &SentLog, first: u16, last: u16) -> Vec<u32> {
	let within = |x: u16, lo: u16, hi: u16| x.wrapping_sub(lo) <= hi.wrapping_sub(lo);
	log.lock()
		.unwrap()
		.iter()
		.filter(|(_, a, b)| within(first, *a, *b) || within(*a, first, last))
		.map(|(ts, _, _)| *ts)
		.collect()
}

/// Dynamic payload type for H.264 (matches the ffmpeg `-payload_type 96` path
/// and `h264.ts`, which assumes PT 96 / 90 kHz without inspecting the PT field).
const PAYLOAD_TYPE: u8 = 96;

/// RTP fixed header length (no CSRCs, no extension).
const RTP_HEADER_LEN: usize = 12;

/// FU-A NAL unit type (RFC 6184 §5.8) — H.264.
const NAL_FU_A: u8 = 28;

/// HEVC fragmentation-unit NAL unit type (RFC 7798 §4.4.3). Carried in the 2-byte HEVC NAL
/// header's `nal_unit_type` field (bits 1..=6 of byte 0), NOT the low 5 bits like H.264.
const HEVC_NAL_FU: u8 = 49;

/// Per-packet UDP payload budget. 1200 keeps a whole RTP packet inside a typical
/// 1500-byte Ethernet MTU with comfortable room for IPv4/IPv6 + UDP headers (the
/// usual streaming default, same ballpark ffmpeg uses). A NAL whose RTP packet
/// would exceed this is FU-A fragmented.
const MTU: usize = 1200;

/// Largest single-NAL payload that still fits the MTU once the RTP header is added.
const MAX_SINGLE_NAL: usize = MTU - RTP_HEADER_LEN;

/// Per-FU-A-fragment NAL payload budget: MTU minus the RTP header (12) minus the
/// 2-byte FU indicator + FU header.
const MAX_FU_PAYLOAD: usize = MTU - RTP_HEADER_LEN - 2;

/// Per-HEVC-FU-fragment NAL payload budget: MTU minus the RTP header (12) minus the 3-byte
/// HEVC FU prefix (2-byte PayloadHdr + 1-byte FU header).
const MAX_FU_HEVC_PAYLOAD: usize = MTU - RTP_HEADER_LEN - 3;

/// Per-AV1-packet OBU payload budget: MTU minus the RTP header (12), the 1-byte AV1 aggregation
/// header, and up to 2 bytes of LEB128 element-length prefix. A chunk this size is < 16384, so
/// its length always LEB128-encodes in ≤ 2 bytes — the 2-byte reservation is never exceeded.
const MAX_AV1_OBU_PAYLOAD: usize = MTU - RTP_HEADER_LEN - 1 - 2;

/// An RTP/H.264 sender bound to one UDP socket, `connect()`ed to the client so
/// `send()` needs no destination per call. Holds the rolling sequence number and
/// the random SSRC for the lifetime of the stream.
pub struct RtpSender {
	socket: UdpSocket,
	/// Codec of the stream — selects the FU fragmentation rules. H.264 uses RFC 6184 FU-A
	/// (1-byte NAL header, type 28); HEVC uses RFC 7798 FU (2-byte NAL header, type 49). Using
	/// the H.264 rules for an HEVC NAL corrupts the 2-byte header (the FU indicator overwrites
	/// `nal_unit_type` → the decoder sees a bogus NAL type 30 / "Multi-layer HEVC" and the IDR
	/// never reassembles), which is exactly the "h265 freezes" bug. AV1 is an OBU stream (no
	/// Annex-B NALs), so it bypasses NAL splitting entirely and takes the dedicated `send_av1_tu`
	/// OBU packetizer (aomedia AV1 RTP format) instead.
	codec: Codec,
	ssrc: u32,
	seq: u16,
	/// Scratch buffer reused for every packet to avoid per-packet allocation.
	buf: Vec<u8>,
	// --- Stage-1 packet pacing (inert until `enable_pacing`) ---
	/// When true, pace every packet through a token bucket ([`SendPacer`]) so a big IDR doesn't
	/// burst-saturate the link (the ~142ms/GOP delivery gap). false = burst.
	pace: bool,
	fps: u32,
	/// Live target bitrate (kbps), shared with the encoder so pacing tracks the (adaptive) bitrate.
	bitrate_kbps: Arc<AtomicU32>,
	/// The bucket; its state carries across AUs (a frame right behind a keyframe waits for the
	/// budget the keyframe used, instead of getting a fresh one).
	pacer: SendPacer,
	/// High-res sleep for the pacing gate (Windows only; the native sender is Windows-only).
	#[cfg(windows)]
	timer: Option<crate::dxgi::platform::HiResTimer>,
	/// What went out, per access unit (see [`SentLog`]).
	sent: SentLog,
}

impl RtpSender {
	/// Bind an ephemeral UDP socket and connect it to `dest`. After this, every
	/// datagram goes to `dest` (the client's RTP port). A random non-zero SSRC and
	/// a random initial sequence number are chosen per RFC 3550 §5.1.
	pub fn new(dest: SocketAddr) -> io::Result<Self> {
		Self::new_with_codec(dest, Codec::H264)
	}

	/// Like `new` but selects the FU fragmentation rules for `codec` (HEVC needs RFC 7798 FU,
	/// not the H.264 FU-A used for `Codec::H264`).
	pub fn new_with_codec(dest: SocketAddr, codec: Codec) -> io::Result<Self> {
		// Bind a wildcard address of the same family as the destination so the
		// socket can reach it (an IPv4 dest needs an IPv4-bound socket).
		let bind_addr: SocketAddr = if dest.is_ipv4() {
			"0.0.0.0:0".parse().unwrap()
		} else {
			"[::]:0".parse().unwrap()
		};
		let socket = UdpSocket::bind(bind_addr)?;
		socket.connect(dest)?;

		// FIX B: enlarge the UDP send buffer. A keyframe/IDR access unit FU-A-fragments into
		// hundreds of ~1200-byte packets; on the default (~64 KiB) send buffer the back-to-back
		// `send()` loop BLOCKS once the buffer fills, stalling the (synchronous) capture+encode
		// thread for ~110 ms every GOP — which reached the Pi as a periodic delivery gap (decode
		// max_gap spikes, Recv-Q~0, 0 loss). A 4 MiB send buffer absorbs the whole burst so the
		// sends return immediately and the kernel drains to the NIC without stalling capture.
		// Best-effort: the OS may clamp the request; never fail construction over a socket tweak.
		let _ = socket2::SockRef::from(&socket).set_send_buffer_size(4 * 1024 * 1024);

		let mut rng = rand::rng();
		// SSRC is any 32-bit value; the client doesn't demux by it but uses it as
		// an opaque per-stream id. Avoid 0 (some stacks treat it specially).
		let ssrc = loop {
			let s: u32 = rng.random();
			if s != 0 {
				break s;
			}
		};
		// RFC 3550 recommends a random starting sequence number.
		let seq: u16 = rng.random();

		Ok(Self {
			socket,
			codec,
			ssrc,
			seq,
			buf: Vec::with_capacity(MTU),
			pace: false,
			fps: 60,
			bitrate_kbps: Arc::new(AtomicU32::new(20_000)),
			pacer: SendPacer::new(),
			#[cfg(windows)]
			timer: None,
			sent: Arc::new(Mutex::new(VecDeque::with_capacity(SENT_LOG_LEN))),
		})
	}

	/// The per-AU send log (shared; see [`SentLog`]).
	pub fn sent_log(&self) -> SentLog {
		self.sent.clone()
	}

	/// Enable Stage-1 packet pacing (called by `RtpEgress` on the host). `bitrate_kbps` is shared
	/// with the encoder so pacing tracks the live (adaptive) bitrate. Creates the high-res sleep
	/// timer on Windows.
	pub fn enable_pacing(&mut self, fps: u32, bitrate_kbps: Arc<AtomicU32>) {
		self.pace = true;
		self.fps = fps.max(1);
		self.bitrate_kbps = bitrate_kbps;
		#[cfg(windows)]
		{
			self.timer = unsafe { crate::dxgi::platform::HiResTimer::new().ok() };
		}
	}

	/// Pacing gate — called right before each packet's `send` (the packet is already in
	/// `self.buf`). Sleeps until the bucket holds that packet's bytes, so packets leave one by one
	/// at the pace rate once the burst budget is spent (the old gate slept only every 48th packet,
	/// so each 57 KB batch still left as one line-rate burst). No-op when pacing is off.
	fn pace_gate(&mut self) {
		if !self.pace {
			return;
		}
		let cost = self.buf.len() as f64;
		let d = self.pacer.wait(cost, Instant::now());
		if !d.is_zero() {
			#[cfg(windows)]
			{
				if let Some(t) = self.timer.as_ref() {
					unsafe { t.sleep_for(d) };
				}
			}
			#[cfg(not(windows))]
			{
				std::thread::sleep(d);
			}
		}
		self.pacer.take(cost, Instant::now());
	}

	/// Pacing setup at the top of an AU of `bytes` wire bytes that was handed over at `arrival`.
	fn pace_begin(&mut self, bytes: usize, arrival: Instant) {
		if self.pace {
			let target = self.bitrate_kbps.load(Ordering::Relaxed);
			self.pacer.begin_au(bytes as f64, target, self.fps, arrival, Instant::now());
		}
	}

	/// The SSRC chosen for this stream (exposed for SDP/diagnostics).
	pub fn ssrc(&self) -> u32 {
		self.ssrc
	}

	/// Packetize and send one full access unit. `annexb` is the Annex-B byte
	/// stream for the AU (a `00 00 00 01` / `00 00 01` start code before each
	/// NAL — exactly what the host's `dump_extra`'d encoder emits, with SPS/PPS
	/// preceding IDRs). `pts_90k` is the presentation timestamp in 90 kHz ticks,
	/// the same value for every packet of this AU.
	///
	/// Each NAL becomes one single-NAL RTP packet if it fits the MTU, else a run
	/// of FU-A fragments. The marker bit is set on the very last packet so the
	/// client decodes the AU as a unit. Returns the number of RTP packets sent.
	pub fn send_access_unit(&mut self, annexb: &[u8], pts_90k: u32) -> io::Result<usize> {
		self.send_access_unit_from(annexb, pts_90k, Instant::now())
	}

	/// [`send_access_unit`](Self::send_access_unit) for an AU the encoder handed over at
	/// `arrival` (the egress mailbox enqueue time): the pacer's spread deadline counts from
	/// then, so time an AU spent queued behind a paced keyframe is caught up, not added.
	fn send_access_unit_from(
		&mut self,
		annexb: &[u8],
		pts_90k: u32,
		arrival: Instant,
	) -> io::Result<usize> {
		let first = self.seq;
		let r = self.send_au_inner(annexb, pts_90k, arrival);
		if self.seq != first {
			let mut log = self.sent.lock().unwrap();
			if log.len() >= SENT_LOG_LEN {
				log.pop_front();
			}
			log.push_back((pts_90k, first, self.seq.wrapping_sub(1)));
		}
		r
	}

	fn send_au_inner(&mut self, annexb: &[u8], pts_90k: u32, arrival: Instant) -> io::Result<usize> {
		// AV1 is a low-overhead OBU temporal-unit stream (no Annex-B start codes), so `split_nals`
		// would return nothing (or bogus pseudo-NALs on a coincidental `00 00 01` in an OBU payload,
		// since AV1 has no emulation prevention) → zero packets on the wire. Packetize it via the
		// aomedia AV1 RTP format the clients expect (`send_av1_tu`), NOT NAL splitting. Here `annexb`
		// is actually the raw OBU TU the encoder emitted.
		if self.codec == Codec::Av1 {
			return self.send_av1_tu(annexb, pts_90k, arrival);
		}
		// Collect the NAL byte-ranges first so we know which one is last (the
		// marker bit must land on the final RTP packet of the AU).
		let nals = split_nals(annexb);
		if nals.is_empty() {
			return Ok(0);
		}
		// Stage-1 pacing setup (see `SendPacer::begin_au`): the AU's wire size (payload + one
		// RTP/FU header per packet) sets how fast the part beyond the burst budget must drip so
		// the AU still leaves within ~1.5 frame intervals of its arrival.
		if self.pace {
			let total = count_packets(&nals, self.codec).max(1) as usize;
			self.pace_begin(annexb.len() + total * (RTP_HEADER_LEN + 3), arrival);
		}
		let last_idx = nals.len() - 1;

		let mut sent = 0;
		for (i, nal) in nals.iter().enumerate() {
			if nal.is_empty() {
				continue;
			}
			let is_last_nal = i == last_idx;
			if nal.len() <= MAX_SINGLE_NAL {
				// Single-NAL: payload is the NAL verbatim (its 1-byte H.264 / 2-byte HEVC header
				// passes through untouched). Marker on the AU's last NAL.
				self.send_packet(nal, pts_90k, is_last_nal)?;
				sent += 1;
			} else if self.codec == Codec::H265 {
				sent += self.send_fu_hevc(nal, pts_90k, is_last_nal)?;
			} else {
				// H.264 (and AV1, which never reaches here — its OBU stream has no >MTU Annex-B NALs).
				sent += self.send_fu_a(nal, pts_90k, is_last_nal)?;
			}
		}
		Ok(sent)
	}

	/// Packetize and send one AV1 temporal unit. `tu` is the low-overhead OBU byte stream NVENC
	/// emits (NO Annex-B start codes; each OBU carries its own `obu_size` field). We frame it with
	/// the aomedia "RTP Payload Format for AV1" aggregation header the Pulsar clients depacketize
	/// (mobile `rtp::process_av1`, `pulsar-render` `push_av1`): the TU is split into MTU-sized
	/// chunks, each sent as ONE length-prefixed OBU *element* under a `W=0` aggregation header
	/// (`Z=0 Y=0 N=0` → the byte `0x00`). Both clients reassemble a `W=0` stream by pure byte
	/// concatenation, so an arbitrary byte split reproduces the exact TU with no OBU parsing needed.
	///
	/// `W=0` is deliberate and load-bearing: the two client depacketizers disagree on the `W`
	/// field's width/position (mobile reads a 3-bit `W` at bit 3 with `N` at bit 2; pulsar-render
	/// reads a 2-bit `W` at bit 4 with `N` at bit 3), but an all-zero aggregation header is `W=0`
	/// and `N=0` under BOTH layouts, making it the only cross-client-safe framing. The RTP marker
	/// bit lands on the final chunk so the depacketizer emits the whole TU as one access unit.
	fn send_av1_tu(&mut self, tu: &[u8], pts_90k: u32, arrival: Instant) -> io::Result<usize> {
		if tu.is_empty() {
			return Ok(0);
		}
		// Stage-1 pacing setup (identical rate model to the NAL path): the chunk count gives the
		// TU's wire size so a big key-frame TU doesn't burst-saturate the link.
		if self.pace {
			let total = ((tu.len() + MAX_AV1_OBU_PAYLOAD - 1) / MAX_AV1_OBU_PAYLOAD).max(1);
			self.pace_begin(tu.len() + total * (RTP_HEADER_LEN + 3), arrival);
		}

		let n = tu.len();
		let mut off = 0usize;
		let mut sent = 0usize;
		while off < n {
			let end = (off + MAX_AV1_OBU_PAYLOAD).min(n);
			let is_last = end == n;
			let chunk = &tu[off..end];

			self.buf.clear();
			self.write_rtp_header(pts_90k, is_last);
			self.buf.push(0x00); // aggregation header: Z=0 Y=0 W=0 N=0 (cross-client safe)
			write_leb128(&mut self.buf, chunk.len()); // W=0 ⇒ every element is length-prefixed
			self.buf.extend_from_slice(chunk);
			self.pace_gate();
			self.socket.send(&self.buf)?;
			self.seq = self.seq.wrapping_add(1);

			off = end;
			sent += 1;
		}
		Ok(sent)
	}

	/// FU-A fragment a single NAL that exceeds the MTU. The original NAL header
	/// byte (`nal[0]`) supplies F+NRI (top 3 bits) and the type (low 5 bits); the
	/// RBSP `nal[1..]` is sliced into MTU-sized fragments. Marker on the final
	/// fragment only if this is the AU's last NAL.
	fn send_fu_a(&mut self, nal: &[u8], pts_90k: u32, is_last_nal: bool) -> io::Result<usize> {
		let hdr = nal[0];
		let fu_indicator = (hdr & 0xE0) | NAL_FU_A; // F + NRI + type=28
		let nal_type = hdr & 0x1F;
		let rbsp = &nal[1..]; // FU-A does NOT resend the original header byte

		let total = rbsp.len();
		let mut off = 0;
		let mut sent = 0;
		while off < total {
			let end = (off + MAX_FU_PAYLOAD).min(total);
			let is_first_frag = off == 0;
			let is_last_frag = end == total;

			let mut fu_header = nal_type;
			if is_first_frag {
				fu_header |= 0x80; // S bit
			}
			if is_last_frag {
				fu_header |= 0x40; // E bit
			}

			// Marker only on the last packet of the AU = last fragment of last NAL.
			let marker = is_last_nal && is_last_frag;

			self.buf.clear();
			self.write_rtp_header(pts_90k, marker);
			self.buf.push(fu_indicator);
			self.buf.push(fu_header);
			self.buf.extend_from_slice(&rbsp[off..end]);
			self.pace_gate();
			self.socket.send(&self.buf)?;
			self.seq = self.seq.wrapping_add(1);

			off = end;
			sent += 1;
		}
		Ok(sent)
	}

	/// FU fragment a single HEVC NAL that exceeds the MTU (RFC 7798 §4.4.3). HEVC NALs carry a
	/// **2-byte** header `nal[0..2]` = `forbidden(1) type(6) layer_id(6) tid_plus1(3)`. The FU
	/// payload header is the original 2-byte header with `nal_unit_type` rewritten to 49 (FU),
	/// keeping `nuh_layer_id` + `nuh_temporal_id_plus1` intact; a 1-byte FU header
	/// `(S<<7)|(E<<6)|orig_type` follows. The RBSP `nal[2..]` (both header bytes stripped) is
	/// sliced into MTU-sized fragments. Mirrors the depacketizer in
	/// `pulsar-render/src/stream/rtp.rs::push_h265` (type 49) and `src/lib/h265.ts`.
	fn send_fu_hevc(&mut self, nal: &[u8], pts_90k: u32, is_last_nal: bool) -> io::Result<usize> {
		// A NAL > MTU always has its 2-byte header (length >> 2); guard anyway.
		if nal.len() < 2 {
			return self.send_fu_a(nal, pts_90k, is_last_nal);
		}
		let b0 = nal[0];
		let b1 = nal[1];
		let orig_type = (b0 >> 1) & 0x3f; // HEVC nal_unit_type (bits 1..=6 of byte 0)
		// FU PayloadHdr = original 2-byte header with nal_unit_type replaced by 49 (FU). Preserve
		// forbidden_zero_bit + the layer_id bit in b0 and the whole b1 (layer_id low + tid+1).
		let ph0 = (b0 & 0x81) | (HEVC_NAL_FU << 1); // keep forbidden(bit7) + layer_id MSB(bit0)
		let ph1 = b1; // nuh_layer_id low 5 bits + nuh_temporal_id_plus1 — unchanged

		let rbsp = &nal[2..]; // FU does NOT resend the 2-byte NAL header
		let total = rbsp.len();
		let mut off = 0;
		let mut sent = 0;
		while off < total {
			let end = (off + MAX_FU_HEVC_PAYLOAD).min(total);
			let is_first_frag = off == 0;
			let is_last_frag = end == total;

			let mut fu_header = orig_type;
			if is_first_frag {
				fu_header |= 0x80; // S bit
			}
			if is_last_frag {
				fu_header |= 0x40; // E bit
			}

			// Marker only on the last packet of the AU = last fragment of last NAL.
			let marker = is_last_nal && is_last_frag;

			self.buf.clear();
			self.write_rtp_header(pts_90k, marker);
			self.buf.push(ph0);
			self.buf.push(ph1);
			self.buf.push(fu_header);
			self.buf.extend_from_slice(&rbsp[off..end]);
			self.pace_gate();
			self.socket.send(&self.buf)?;
			self.seq = self.seq.wrapping_add(1);

			off = end;
			sent += 1;
		}
		Ok(sent)
	}

	/// Emit one single-NAL RTP packet whose payload is `payload` (a whole NAL).
	fn send_packet(&mut self, payload: &[u8], pts_90k: u32, marker: bool) -> io::Result<()> {
		self.buf.clear();
		self.write_rtp_header(pts_90k, marker);
		self.buf.extend_from_slice(payload);
		self.pace_gate();
		self.socket.send(&self.buf)?;
		self.seq = self.seq.wrapping_add(1);
		Ok(())
	}

	/// Append the 12-byte RTP fixed header to `self.buf`. V=2, P=0, X=0, CC=0;
	/// marker + PT in byte 1; big-endian sequence, timestamp and SSRC.
	fn write_rtp_header(&mut self, pts_90k: u32, marker: bool) {
		let byte0 = 0x80; // version 2, no padding/extension/CSRC
		let byte1 = if marker {
			0x80 | PAYLOAD_TYPE
		} else {
			PAYLOAD_TYPE
		};
		self.buf.push(byte0);
		self.buf.push(byte1);
		self.buf.extend_from_slice(&self.seq.to_be_bytes());
		self.buf.extend_from_slice(&pts_90k.to_be_bytes());
		self.buf.extend_from_slice(&self.ssrc.to_be_bytes());
	}
}

// ===========================================================================
// RtpEgress — decouple the blocking UDP send from the capture+encode thread.
// ===========================================================================
//
// PROBLEM (the opi5 "periodic ~110 ms freeze → jump"): `Encoder::submit` used to call
// `RtpSender::send_access_unit` INLINE on the capture+encode thread. An IDR/keyframe AU
// FU-A-fragments into hundreds of ~1200-byte packets; the back-to-back blocking
// `UdpSocket::send` loop wedges that thread until the kernel send buffer drains (~110 ms
// every GOP, and the OS may clamp SO_SNDBUF so the 4 MiB request doesn't always help), so
// frame PRODUCTION halts. The Pi sees a periodic delivery gap (decode max_gap spikes, 0
// loss, empty recv-Q) and its newest-wins presenter collapses the post-gap catch-up burst
// into one visual jump (the cursor teleport / typing-in-bursts).
//
// FIX (Sunshine's model — `videoBroadcastThread` + `safe::queue_t`, _ref/sunshine
// stream.cpp:1272/1297 + thread_safe.h:249-273): the encode thread only ENQUEUES the owned
// Annex-B AU (a memcpy + a brief lock) and returns; a dedicated `pulsar-rtp-send` thread is
// the SOLE owner of the `RtpSender` and runs the blocking send loop. A slow/wedged send can
// no longer back-pressure NVENC. The bounded mailbox drops the stale backlog on overflow
// (newest-wins — exactly Sunshine's `safe::queue_t` clear-on-overflow), so the producer
// never blocks and the queue never grows unbounded; a dropped AU recovers on the next frame
// / the multi-second safety IDR. A single consumer over a FIFO keeps the wire sequence
// numbers monotonic, and the bytes are produced by the SAME unchanged `RtpSender`, so the
// client depacketizer (`src/lib/h264.ts`, the Pi ffmpeg demuxer) is untouched.

/// One encoded access unit handed encode-thread → sender-thread (owned copy of the bytes,
/// since the NVENC locked bitstream is only valid until Unlock).
struct AuMsg {
	annexb: Vec<u8>,
	pts_90k: u32,
	/// When the encoder handed it over (the pacer's spread deadline counts from here).
	at: Instant,
}

/// Bounded producer→consumer mailbox (mutex + condvar + `VecDeque`), mirroring Sunshine's
/// `safe::queue_t`: enqueue only emplaces (never waits on the network), and on overflow it
/// drops the stale backlog and keeps the newest AU.
struct AuMailbox {
	state: Mutex<MailState>,
	cv: Condvar,
	cap: usize,
}
struct MailState {
	q: VecDeque<AuMsg>,
	stop: bool,
}

/// Decoupled RTP egress: owns the dedicated `pulsar-rtp-send` thread that is the sole owner
/// of the inner `RtpSender`. Replaces the inline `RtpSender` on `Encoder` so the per-GOP
/// ~110 ms encode-thread stall is gone. `Drop` closes the mailbox and joins the thread so the
/// socket + thread never orphan.
pub struct RtpEgress {
	mb: Arc<AuMailbox>,
	ssrc: u32,
	sent: SentLog,
	join: Option<JoinHandle<()>>,
	/// A/B knob: `PULSAR_RTP_INLINE=1` keeps the OLD behavior (send on the calling/encode
	/// thread) to reproduce the stall on demand. `Some` ⇒ inline, no sender thread spawned.
	inline: Option<Mutex<RtpSender>>,
}

impl RtpEgress {
	/// Bind the RTP socket (unchanged `RtpSender::new`) and, unless `PULSAR_RTP_INLINE=1`,
	/// spawn the sender thread that drains the mailbox. `PULSAR_RTP_QCAP` overrides the
	/// mailbox depth (default 3 access units).
	pub fn spawn(
		dest: SocketAddr,
		codec: Codec,
		fps: u32,
		bitrate_kbps: Arc<AtomicU32>,
	) -> io::Result<Self> {
		let mut sender = RtpSender::new_with_codec(dest, codec)?;
		// Bound a blocked send so a wedged socket can never hang teardown: the sender thread
		// checks `stop` between AUs, and the write timeout caps any in-flight send. 250 ms is
		// far longer than any healthy AU send yet imperceptible on stop. On timeout the rest of
		// that AU is dropped (returned as an Err the sender loop ignores).
		let _ = sender
			.socket
			.set_write_timeout(Some(Duration::from_millis(250)));
		let ssrc = sender.ssrc();
		let sent = sender.sent_log();

		// A/B: inline mode reproduces the old inline-send stall (no sender thread).
		if std::env::var("PULSAR_RTP_INLINE")
			.map(|v| v == "1" || v == "on" || v == "true")
			.unwrap_or(false)
		{
			return Ok(Self {
				mb: Arc::new(AuMailbox {
					state: Mutex::new(MailState {
						q: VecDeque::new(),
						stop: false,
					}),
					cv: Condvar::new(),
					cap: 1,
				}),
				ssrc,
				sent,
				join: None,
				inline: Some(Mutex::new(sender)),
			});
		}

		// Stage-1 packet pacing: ON by default on the threaded path toward a real (remote)
		// destination (PULSAR_RTP_PACE=0 disables for A/B). Spreads each AU's packets so a big IDR
		// doesn't burst-saturate the link. A LOOPBACK destination is the host's media-over-session
		// intake: its forwarder (src-tauri host/handlers.rs) paces the actual wire, in one budget
		// with the FEC parity and NACK retransmits it adds — pacing here as well would only stack
		// a second queue (added latency) in front of that one. PULSAR_RTP_PACE=1 forces it on.
		let pace = match std::env::var("PULSAR_RTP_PACE") {
			Ok(v) => v != "0",
			Err(_) => !dest.ip().is_loopback(),
		};
		if pace {
			sender.enable_pacing(fps, bitrate_kbps);
		}
		// A19: a SHALLOW egress mailbox (3 AUs, was 16). The mailbox only buffers AUs the sender
		// thread hasn't put on the wire yet; on overflow it drops the STALE backlog (newest-wins),
		// so a deep queue just lets old frames pile up and adds delivery latency before they're
		// dropped anyway. 3 absorbs a brief send hiccup without hoarding stale frames. Still
		// env-overridable via PULSAR_RTP_QCAP.
		let cap = std::env::var("PULSAR_RTP_QCAP")
			.ok()
			.and_then(|v| v.parse::<usize>().ok())
			.filter(|n| *n > 0)
			.unwrap_or(3);
		let mb = Arc::new(AuMailbox {
			state: Mutex::new(MailState {
				q: VecDeque::with_capacity(cap),
				stop: false,
			}),
			cv: Condvar::new(),
			cap,
		});
		let mb_thread = mb.clone();
		let join = std::thread::Builder::new()
			.name("pulsar-rtp-send".into())
			.spawn(move || {
				// A4: raise THIS sender thread to THREAD_PRIORITY_HIGHEST so the OS schedules its
				// blocking UDP send promptly under load (the IDR packet burst must drain fast). NOT
				// TIME_CRITICAL on purpose: that would let a long send loop starve the capture/encode
				// thread (which already runs TIME_CRITICAL) — we want the sender just below it.
				#[cfg(windows)]
				unsafe {
					use windows::Win32::System::Threading::{
						GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
					};
					let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
				}
				let mut sender = sender;
				loop {
					// Wait for the next AU (or a stop). Holding the lock only across the
					// pop/wait — never across the send below — so the producer never blocks
					// on the network.
					let msg = {
						let mut st = mb_thread.state.lock().unwrap();
						loop {
							if let Some(m) = st.q.pop_front() {
								break Some(m);
							}
							if st.stop {
								break None;
							}
							st = mb_thread.cv.wait(st).unwrap();
						}
					};
					match msg {
						// An Err (incl. a write-timeout on a saturated socket) just drops the
						// rest of THIS AU and moves on — never stalls, never reorders.
						Some(m) => {
							let _ = sender.send_access_unit_from(&m.annexb, m.pts_90k, m.at);
						}
						None => break,
					}
				}
			})?;
		Ok(Self {
			mb,
			ssrc,
			sent,
			join: Some(join),
			inline: None,
		})
	}

	/// The stream SSRC (cached at construction so SDP/diagnostics need not cross the thread).
	pub fn ssrc(&self) -> u32 {
		self.ssrc
	}

	/// RTP timestamps of the access units already sent whose packets overlap `first..=last`.
	pub fn frames_in_seq_range(&self, first: u16, last: u16) -> Vec<u32> {
		frames_in_seq_range(&self.sent, first, last)
	}

	/// Hand one access unit to the sender thread. NON-BLOCKING: copies `annexb` (valid only
	/// until the caller unlocks the NVENC bitstream) into an owned `Vec`, enqueues it, and
	/// returns. On overflow the stale backlog is dropped (newest-wins) so the encode thread is
	/// never back-pressured. In `PULSAR_RTP_INLINE=1` mode it sends synchronously here (the old
	/// stall-prone path, for A/B).
	pub fn send_access_unit(&self, annexb: &[u8], pts_90k: u32) {
		if let Some(inline) = &self.inline {
			let mut s = inline.lock().unwrap();
			let _ = s.send_access_unit(annexb, pts_90k);
			return;
		}
		let msg = AuMsg {
			annexb: annexb.to_vec(),
			pts_90k,
			at: Instant::now(),
		};
		{
			let mut st = self.mb.state.lock().unwrap();
			if st.q.len() >= self.mb.cap {
				// Sunshine `safe::queue_t` overflow policy: drop the stale backlog, keep newest.
				st.q.clear();
			}
			st.q.push_back(msg);
		}
		self.mb.cv.notify_one();
	}
}

impl Drop for RtpEgress {
	fn drop(&mut self) {
		// Wake the sender thread (if any) and join it so the socket + thread never orphan.
		{
			let mut st = self.mb.state.lock().unwrap();
			st.stop = true;
		}
		self.mb.cv.notify_all();
		if let Some(h) = self.join.take() {
			let _ = h.join();
		}
	}
}

/// Count how many RTP packets `send_access_unit` will emit for these NALs (single-NAL = 1 each,
/// else `ceil(rbsp_len / MAX_FU_PAYLOAD)` FU-A fragments). Used to size the AU for the pacer
/// (payload + per-packet headers).
fn count_packets(nals: &[&[u8]], codec: Codec) -> u32 {
	// HEVC strips a 2-byte NAL header and uses a 3-byte FU prefix; H.264 strips 1 byte / 2-byte prefix.
	let (hdr, budget) = if codec == Codec::H265 {
		(2usize, MAX_FU_HEVC_PAYLOAD)
	} else {
		(1usize, MAX_FU_PAYLOAD)
	};
	nals.iter()
		.map(|nal| {
			if nal.is_empty() {
				0
			} else if nal.len() <= MAX_SINGLE_NAL {
				1
			} else {
				// FU fragments the RBSP (nal[hdr..]); ceil-divide by the per-fragment budget.
				((nal.len() - hdr + budget - 1) / budget) as u32
			}
		})
		.sum()
}

/// Pace = this x the target bitrate: at steady state a frame's packets spread over about half
/// a frame interval, with headroom for the encoder's frame-size variance.
const PACE_FACTOR: f64 = 2.0;
/// Never pace slower than this: at very low targets the per-packet overhead and encoder
/// overshoot dominate, and smoothing to 2 Mbit/s is still far gentler than a line-rate burst.
const PACE_FLOOR_KBPS: f64 = 2000.0;
/// Burst budget (bucket depth) in ms of the pace rate. Covers whole P-frames on a fast LAN
/// (no added delay there) and the first slice of a keyframe on a slow link.
const PACE_BURST_MS: f64 = 16.0;
/// ...but at least ~4 full packets, so a tiny rate still lets a small frame out in one go.
const PACE_BURST_MIN: f64 = 4.0 * MTU as f64;
/// A packet may leave while the bucket is short by up to this many ms of rate (repaid by the
/// refill): a sleep shorter than the timer's ~1 ms granularity is not worth taking, so the few
/// packets falling due inside one millisecond go together.
const PACE_BORROW_MS: f64 = 1.0;
/// The part of an AU beyond the burst budget is spread so the AU is out within this many frame
/// intervals of its arrival. The old pacer fit every AU into one interval, which at 120 fps
/// still fired a keyframe at several times the link rate; much more than 1.5 would let the
/// egress mailbox (3 AUs, drops on overflow) fill behind a keyframe.
const AU_SPREAD_FRAMES: f64 = 1.5;
/// Least time an AU that starts late (it waited behind a keyframe) gets: it drains at up to
/// 2x the frame cadence, so a backlog shrinks instead of growing frame over frame.
const AU_MIN_WINDOW_FRAMES: f64 = 0.5;

/// Sender-side pacer: a packet-granular token bucket. Pure — every call takes `now` — so the
/// tests drive it with a fake clock.
///
/// - Base rate = `PACE_FACTOR` x the live target (>= `PACE_FLOOR_KBPS`), in bytes per ms.
/// - Depth = `PACE_BURST_MS` of the base rate: a frame after an idle gap leaves at once up to
///   that budget; the bucket state carries across AUs, so the frame behind a keyframe waits for
///   the budget the keyframe used.
/// - Per AU the drain rate rises above the base just enough for the AU to be out within
///   `AU_SPREAD_FRAMES` intervals of its arrival (at least `AU_MIN_WINDOW_FRAMES` from now):
///   the queue delay behind a keyframe stays bounded instead of growing frame over frame.
struct SendPacer {
	tokens: f64,
	/// `None` until the first AU (the bucket starts full).
	last: Option<Instant>,
	/// Base rate (bytes per ms) — sets the depth.
	base: f64,
	/// Drain rate for the current AU (>= base).
	rate: f64,
}

impl SendPacer {
	fn new() -> Self {
		Self { tokens: 0.0, last: None, base: 0.0, rate: 0.0 }
	}

	fn depth(&self) -> f64 {
		(PACE_BURST_MS * self.base).max(PACE_BURST_MIN)
	}

	fn refill(&mut self, now: Instant) {
		if let Some(last) = self.last {
			let dt = now.saturating_duration_since(last).as_secs_f64() * 1000.0;
			self.tokens = (self.tokens + self.rate * dt).min(self.depth());
		} else {
			self.tokens = self.depth();
		}
		if self.last.map_or(true, |l| now > l) {
			self.last = Some(now);
		}
	}

	/// Start an AU of `bytes` wire bytes handed over at `arrival` (`now` = when its first packet
	/// is about to go). `target_kbps` 0 = unknown: the floor rate applies.
	fn begin_au(&mut self, bytes: f64, target_kbps: u32, fps: u32, arrival: Instant, now: Instant) {
		let base = (PACE_FACTOR * target_kbps as f64 / 8.0).max(PACE_FLOOR_KBPS / 8.0);
		if self.last.is_none() {
			// First AU: the bucket starts full at the new rate's depth.
			self.base = base;
			self.rate = base;
			self.tokens = self.depth();
			self.last = Some(now);
		} else {
			// Accrue the idle gap at the previous AU's rate, then switch to the new base.
			self.refill(now);
			self.base = base;
			self.tokens = self.tokens.min(self.depth());
		}
		let interval = 1000.0 / fps.max(1) as f64;
		let since_arrival = now.saturating_duration_since(arrival).as_secs_f64() * 1000.0;
		let window = (AU_SPREAD_FRAMES * interval - since_arrival).max(AU_MIN_WINDOW_FRAMES * interval);
		// A debt left by the previous AU's last packets counts against this one.
		let excess = (bytes - self.tokens).max(0.0);
		self.rate = self.base.max(excess / window);
	}

	/// How long to wait before a `cost`-byte packet may leave (zero = now).
	fn wait(&mut self, cost: f64, now: Instant) -> Duration {
		self.refill(now);
		let deficit = cost - self.tokens - self.rate * PACE_BORROW_MS;
		if deficit <= 0.0 || self.rate <= 0.0 {
			Duration::ZERO
		} else {
			Duration::from_secs_f64(deficit / self.rate / 1000.0)
		}
	}

	/// The packet left at `now` (after any wait): spend its bytes.
	fn take(&mut self, cost: f64, now: Instant) {
		self.refill(now);
		self.tokens -= cost;
	}
}

/// Split an Annex-B byte stream into its NAL units (without the start codes).
/// Recognizes both 4-byte (`00 00 00 01`) and 3-byte (`00 00 01`) start codes,
/// as emitted by the encoder/`dump_extra`. Trailing zero bytes that some encoders
/// append are trimmed off each NAL.
fn split_nals(data: &[u8]) -> Vec<&[u8]> {
	let mut nals = Vec::new();
	let n = data.len();

	// Find the offset of the first start code; bytes before it (if any) are skipped.
	let mut i = match find_start_code(data, 0) {
		Some((sc_start, _)) if sc_start == 0 => 0,
		Some((_, sc_end)) => sc_end,
		None => return nals, // no start code at all → nothing to send
	};
	// `i` now points at (or just before) the first start code. Normalize: advance
	// past the leading start code to the first NAL byte.
	if let Some((sc_start, sc_end)) = find_start_code(data, i) {
		if sc_start == i {
			i = sc_end;
		}
	}

	while i < n {
		// The NAL runs from `i` up to the next start code (or end of buffer).
		let (nal_end, next) = match find_start_code(data, i) {
			Some((sc_start, sc_end)) => (sc_start, sc_end),
			None => (n, n),
		};
		// Trim trailing zero bytes that belong to the next start code's run-in.
		let mut end = nal_end;
		while end > i && data[end - 1] == 0x00 {
			end -= 1;
		}
		if end > i {
			nals.push(&data[i..end]);
		}
		i = next;
	}
	nals
}

/// Find the next Annex-B start code at or after `from`, returning
/// `(start_index, byte_after_start_code)`. Matches `00 00 01` and `00 00 00 01`;
/// a 4-byte code is reported with its full length consumed.
fn find_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
	let n = data.len();
	let mut i = from;
	while i + 3 <= n {
		if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
			// Prefer the 4-byte form if preceded by an extra zero.
			if i > 0 && data[i - 1] == 0 {
				return Some((i - 1, i + 3));
			}
			return Some((i, i + 3));
		}
		i += 1;
	}
	None
}

/// Append `value` as an unsigned LEB128 integer (AV1 spec §4.10.5) — the OBU-element length
/// prefix the AV1 RTP depacketizers decode with their own `read_leb128` (mobile/pulsar-render).
fn write_leb128(buf: &mut Vec<u8>, mut value: usize) {
	loop {
		let mut byte = (value & 0x7f) as u8;
		value >>= 7;
		if value != 0 {
			byte |= 0x80;
		}
		buf.push(byte);
		if value == 0 {
			break;
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Build the Annex-B for a NAL of `type`/`nri` with `len` RBSP bytes, prefixed
	/// by a 4-byte start code.
	fn annexb_nal(nal_type: u8, nri: u8, rbsp_len: usize) -> Vec<u8> {
		let mut v = vec![0, 0, 0, 1];
		v.push((nri << 5) | nal_type); // [F=0|NRI|type]
		v.extend(std::iter::repeat(0xAB).take(rbsp_len));
		v
	}

	fn parse_header(pkt: &[u8]) -> (bool, u8, u16, u32, u32) {
		let marker = pkt[1] & 0x80 != 0;
		let pt = pkt[1] & 0x7f;
		let seq = u16::from_be_bytes([pkt[2], pkt[3]]);
		let ts = u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]);
		let ssrc = u32::from_be_bytes([pkt[8], pkt[9], pkt[10], pkt[11]]);
		(marker, pt, seq, ts, ssrc)
	}

	/// Reassemble the NALs a depacketizer (mirroring `h264.ts`) would produce from
	/// a list of RTP packets, asserting marker/PT/timestamp invariants.
	fn depacketize(pkts: &[Vec<u8>], expect_ts: u32) -> Vec<Vec<u8>> {
		let mut nals: Vec<Vec<u8>> = Vec::new();
		let mut fu: Option<Vec<u8>> = None;
		let mut last_seq: Option<u16> = None;
		let mut saw_marker = false;
		for pkt in pkts {
			assert!(pkt.len() >= RTP_HEADER_LEN);
			let (marker, pt, seq, ts, _ssrc) = parse_header(pkt);
			assert_eq!(pt, PAYLOAD_TYPE, "payload type must be 96");
			assert_eq!(ts, expect_ts, "timestamp constant across the AU");
			if let Some(prev) = last_seq {
				assert_eq!(seq, prev.wrapping_add(1), "sequence increments by 1");
			}
			last_seq = Some(seq);
			if marker {
				saw_marker = true;
			}

			let pl = &pkt[RTP_HEADER_LEN..];
			let t = pl[0] & 0x1f;
			if (1..=23).contains(&t) {
				nals.push(pl.to_vec());
			} else if t == NAL_FU_A {
				let fh = pl[1];
				let start = fh & 0x80 != 0;
				let endb = fh & 0x40 != 0;
				let orig_type = fh & 0x1f;
				let nri = pl[0] & 0x60;
				if start {
					fu = Some(vec![nri | orig_type]); // reconstruct NAL header
				}
				if let Some(acc) = fu.as_mut() {
					acc.extend_from_slice(&pl[2..]);
					if endb {
						nals.push(fu.take().unwrap());
					}
				}
			}
		}
		assert!(saw_marker, "exactly one packet must carry the marker bit");
		// Marker must be on the LAST packet only.
		for (i, pkt) in pkts.iter().enumerate() {
			let m = pkt[1] & 0x80 != 0;
			assert_eq!(m, i == pkts.len() - 1, "marker only on the final packet");
		}
		nals
	}

	/// Capture what `send_access_unit` puts on the wire by reading from a paired
	/// loopback socket.
	fn roundtrip(annexb: &[u8], pts: u32) -> Vec<Vec<u8>> {
		roundtrip_codec(annexb, pts, Codec::H264)
	}

	fn roundtrip_codec(annexb: &[u8], pts: u32, codec: Codec) -> Vec<Vec<u8>> {
		let recv = UdpSocket::bind("127.0.0.1:0").unwrap();
		let dest = recv.local_addr().unwrap();
		let mut sender = RtpSender::new_with_codec(dest, codec).unwrap();
		let sent = sender.send_access_unit(annexb, pts).unwrap();

		let mut pkts = Vec::new();
		let mut buf = [0u8; 2048];
		recv.set_read_timeout(Some(std::time::Duration::from_secs(2)))
			.unwrap();
		for _ in 0..sent {
			let n = recv.recv(&mut buf).unwrap();
			pkts.push(buf[..n].to_vec());
		}
		pkts
	}

	#[test]
	fn single_nal_one_packet() {
		// A small NAL (type 1, NRI 2) → exactly one single-NAL packet, marker set.
		let au = annexb_nal(1, 2, 100);
		let pkts = roundtrip(&au, 90_000);
		assert_eq!(pkts.len(), 1);
		let nals = depacketize(&pkts, 90_000);
		assert_eq!(nals.len(), 1);
		// Reassembled NAL equals the original (header + RBSP).
		assert_eq!(nals[0], &au[4..]);
		// Header byte preserved exactly: F=0, NRI=2, type=1.
		assert_eq!(nals[0][0], (2 << 5) | 1);
	}

	#[test]
	fn large_nal_fragments_fu_a() {
		// A NAL bigger than the MTU (type 5 IDR, NRI 3) → multiple FU-A fragments.
		let au = annexb_nal(5, 3, MTU * 3);
		let pkts = roundtrip(&au, 12345);
		assert!(pkts.len() > 3, "should fragment into several packets");
		let nals = depacketize(&pkts, 12345);
		assert_eq!(nals.len(), 1);
		// FU-A reconstruction restores the exact original NAL (header + RBSP).
		assert_eq!(nals[0], &au[4..]);
		assert_eq!(nals[0][0], (3 << 5) | 5); // NRI=3, type=5 survived round-trip
	}

	#[test]
	fn multi_nal_access_unit_marker_on_last() {
		// SPS(7) + PPS(8) + IDR(5, large) — the keyframe AU shape `dump_extra` emits.
		let mut au = annexb_nal(7, 3, 20);
		au.extend(annexb_nal(8, 3, 8));
		au.extend(annexb_nal(5, 3, MTU * 2));
		let pkts = roundtrip(&au, 7777);
		let nals = depacketize(&pkts, 7777);
		assert_eq!(nals.len(), 3, "SPS, PPS, IDR all delivered");
		assert_eq!(nals[0][0] & 0x1f, 7);
		assert_eq!(nals[1][0] & 0x1f, 8);
		assert_eq!(nals[2][0] & 0x1f, 5);
	}

	#[test]
	fn three_byte_start_code_is_split() {
		// Mixed 4-byte then 3-byte start codes.
		let mut au = vec![0, 0, 0, 1, (3 << 5) | 7, 0xAA, 0xBB];
		au.extend_from_slice(&[0, 0, 1, (2 << 5) | 1, 0xCC]);
		let pkts = roundtrip(&au, 1);
		let nals = depacketize(&pkts, 1);
		assert_eq!(nals.len(), 2);
		assert_eq!(nals[0], &[(3 << 5) | 7, 0xAA, 0xBB]);
		assert_eq!(nals[1], &[(2 << 5) | 1, 0xCC]);
	}

	#[test]
	fn ssrc_is_random_nonzero_and_stable() {
		let recv = UdpSocket::bind("127.0.0.1:0").unwrap();
		let mut s = RtpSender::new(recv.local_addr().unwrap()).unwrap();
		let ssrc = s.ssrc();
		assert_ne!(ssrc, 0);
		// SSRC is constant across packets in a stream.
		s.send_access_unit(&annexb_nal(1, 2, 10), 0).unwrap();
		let mut buf = [0u8; 2048];
		let n = recv.recv(&mut buf).unwrap();
		let (_, _, _, _, wire_ssrc) = parse_header(&buf[..n]);
		assert_eq!(wire_ssrc, ssrc);
	}

	/// Build the Annex-B for one HEVC NAL: a 2-byte header `forbidden(1) type(6) layer_id(6)
	/// tid+1(3)` + `rbsp_len` body bytes, prefixed by a 4-byte start code.
	fn annexb_hevc_nal(nal_type: u8, layer_id: u8, tid_plus1: u8, rbsp_len: usize) -> Vec<u8> {
		let b0 = ((nal_type & 0x3f) << 1) | ((layer_id >> 5) & 0x01);
		let b1 = ((layer_id & 0x1f) << 3) | (tid_plus1 & 0x07);
		let mut v = vec![0, 0, 0, 1, b0, b1];
		v.extend(std::iter::repeat(0xCD).take(rbsp_len));
		v
	}

	/// Reassemble HEVC NALs from RTP packets, mirroring `pulsar-render/src/stream/rtp.rs::push_h265`
	/// (single NAL ≤47, AP 48, FU 49) — the real Pi depacketizer. Asserts marker/PT/ts invariants.
	fn depacketize_hevc(pkts: &[Vec<u8>], expect_ts: u32) -> Vec<Vec<u8>> {
		let mut nals: Vec<Vec<u8>> = Vec::new();
		let mut fu: Option<Vec<u8>> = None;
		let mut last_seq: Option<u16> = None;
		for pkt in pkts {
			let (_marker, pt, seq, ts, _ssrc) = parse_header(pkt);
			assert_eq!(pt, PAYLOAD_TYPE);
			assert_eq!(ts, expect_ts, "timestamp constant across the AU");
			if let Some(prev) = last_seq {
				assert_eq!(seq, prev.wrapping_add(1), "sequence increments by 1");
			}
			last_seq = Some(seq);
			let pl = &pkt[RTP_HEADER_LEN..];
			let nal_type = (pl[0] >> 1) & 0x3f;
			if nal_type <= 47 {
				nals.push(pl.to_vec());
			} else if nal_type == 49 {
				let fu_hdr0 = pl[0];
				let fu_hdr1 = pl[1];
				let fuhdr = pl[2];
				let start = (fuhdr & 0x80) != 0;
				let endb = (fuhdr & 0x40) != 0;
				let fu_type = fuhdr & 0x3f;
				let layer_id = ((fu_hdr0 & 0x01) << 5) | ((fu_hdr1 >> 3) & 0x1f);
				let tid = fu_hdr1 & 0x07;
				if start {
					let h0 = (fu_type << 1) | ((layer_id >> 5) & 0x01);
					let h1 = ((layer_id & 0x1f) << 3) | (tid & 0x07);
					fu = Some(vec![h0, h1]);
				}
				if let Some(acc) = fu.as_mut() {
					acc.extend_from_slice(&pl[3..]);
					if endb {
						nals.push(fu.take().unwrap());
					}
				}
			} else {
				panic!("unexpected HEVC RTP NAL type {nal_type} (H.264 FU-A leaked into an HEVC stream?)");
			}
		}
		nals
	}

	#[test]
	fn hevc_small_nal_passes_2byte_header_through() {
		// A small HEVC NAL (≤ MTU) must be a single-NAL packet with its 2-byte header intact —
		// no FU-A corruption of nal_unit_type. IDR_W_RADL=19, layer_id=0, tid+1=1.
		let au = annexb_hevc_nal(19, 0, 1, 100);
		let pkts = roundtrip_codec(&au, 7, Codec::H265);
		assert_eq!(pkts.len(), 1);
		let nals = depacketize_hevc(&pkts, 7);
		assert_eq!(nals.len(), 1);
		assert_eq!(nals[0], &au[4..], "2-byte header + RBSP preserved verbatim");
		assert_eq!((nals[0][0] >> 1) & 0x3f, 19, "nal_unit_type stays 19, not 30");
	}

	#[test]
	fn hevc_large_idr_fu_reassembles_to_type_19_not_30() {
		// The regression: a >MTU HEVC IDR used to be FU-A-fragmented with H.264 rules, which
		// overwrote nal_unit_type → the Pi saw NAL type 30 ("Multi-layer HEVC" / "Skipping NAL
		// unit 30") and the IDR never reassembled. With RFC 7798 FU it must rebuild to type 19,
		// layer_id 0, with the EXACT original bytes.
		let au = annexb_hevc_nal(19, 0, 1, MTU * 3);
		let pkts = roundtrip_codec(&au, 4242, Codec::H265);
		assert!(pkts.len() > 3, "a >3·MTU NAL must fragment into several FU packets");
		// Every packet must be an HEVC FU (49), never an H.264 FU-A (28 → type 30 here).
		for pkt in &pkts {
			let nt = (pkt[RTP_HEADER_LEN] >> 1) & 0x3f;
			assert_eq!(nt, 49, "fragments must be HEVC FU (type 49), got {nt}");
		}
		let nals = depacketize_hevc(&pkts, 4242);
		assert_eq!(nals.len(), 1, "all fragments reassemble into one NAL");
		assert_eq!(nals[0], &au[4..], "reassembled NAL == original (header + RBSP)");
		assert_eq!((nals[0][0] >> 1) & 0x3f, 19, "reassembled nal_unit_type is 19 (IDR), not 30");
	}

	#[test]
	fn hevc_preserves_layer_id_and_tid() {
		// nuh_layer_id + nuh_temporal_id_plus1 must survive fragmentation untouched (a layer_id
		// corruption is what made ffmpeg report "Multi-layer HEVC coding is not implemented").
		let au = annexb_hevc_nal(21, 0, 2, MTU * 2); // CRA=21, tid+1=2
		let pkts = roundtrip_codec(&au, 1, Codec::H265);
		let nals = depacketize_hevc(&pkts, 1);
		assert_eq!(nals.len(), 1);
		let layer_id = ((nals[0][0] & 0x01) << 5) | ((nals[0][1] >> 3) & 0x1f);
		let tid = nals[0][1] & 0x07;
		assert_eq!(layer_id, 0, "nuh_layer_id stays 0 (single-layer)");
		assert_eq!(tid, 2, "nuh_temporal_id_plus1 preserved");
	}

	// -------------------------------------------------------------------------
	// AV1 OBU packetization (aomedia AV1 RTP format) tests
	// -------------------------------------------------------------------------

	/// Read a LEB128 unsigned integer from `data[pos..]`, returning `(value, bytes_consumed)`.
	/// Mirrors both clients' `read_leb128` so the test depacketizes exactly as they do.
	fn read_leb128_at(data: &[u8], mut pos: usize) -> (usize, usize) {
		let mut value = 0usize;
		let mut shift = 0u32;
		let start = pos;
		loop {
			let b = data[pos];
			pos += 1;
			value |= ((b & 0x7f) as usize) << shift;
			shift += 7;
			if b & 0x80 == 0 {
				break;
			}
		}
		(value, pos - start)
	}

	/// Build a low-overhead AV1 OBU temporal unit: a sequence-header OBU (type 1) + a frame OBU
	/// (type 6), each with `obu_has_size_field=1` and a LEB128 `obu_size` — the exact shape NVENC's
	/// `outputAnnexBFormat=0` OBU stream has (no Annex-B start codes).
	fn av1_tu(frame_payload_len: usize) -> Vec<u8> {
		let mut tu = Vec::new();
		let seq_payload = [0xAAu8, 0xBB, 0xCC];
		tu.push((1u8 << 3) | 0x02); // obu_type=1 (seq header), has_size=1
		write_leb128(&mut tu, seq_payload.len());
		tu.extend_from_slice(&seq_payload);
		tu.push((6u8 << 3) | 0x02); // obu_type=6 (frame), has_size=1
		write_leb128(&mut tu, frame_payload_len);
		tu.extend(std::iter::repeat(0xE5).take(frame_payload_len));
		tu
	}

	/// Reassemble an AV1 temporal unit from RTP packets the way BOTH Pulsar clients do for a `W=0`
	/// stream (mobile `process_av1` / pulsar-render `push_av1`): skip the 1-byte aggregation header
	/// and concatenate every length-prefixed OBU element. Also asserts the aggregation header is
	/// `W=0`/`N=0` under BOTH clients' (differing) bit layouts, plus the PT/timestamp/seq/marker
	/// invariants the depacketizers rely on.
	fn depacketize_av1(pkts: &[Vec<u8>], expect_ts: u32) -> Vec<u8> {
		let mut tu = Vec::new();
		let mut last_seq: Option<u16> = None;
		for (i, pkt) in pkts.iter().enumerate() {
			assert!(pkt.len() >= RTP_HEADER_LEN);
			let (marker, pt, seq, ts, _ssrc) = parse_header(pkt);
			assert_eq!(pt, PAYLOAD_TYPE, "payload type must be 96");
			assert_eq!(ts, expect_ts, "timestamp constant across the TU");
			assert_eq!(marker, i == pkts.len() - 1, "marker only on the final packet");
			if let Some(prev) = last_seq {
				assert_eq!(seq, prev.wrapping_add(1), "sequence increments by 1");
			}
			last_seq = Some(seq);
			let pl = &pkt[RTP_HEADER_LEN..];
			let agg = pl[0];
			// The whole aggregation byte must be zero: that is the ONLY value that reads as
			// Z=0/Y=0/W=0/N=0 under BOTH client layouts (mobile W@bit3/N@bit2, render W@bit4/N@bit3).
			assert_eq!(agg, 0x00, "AV1 aggregation header must be 0x00 for cross-client compat");
			let mut p = 1;
			while p < pl.len() {
				let (len, consumed) = read_leb128_at(pl, p);
				p += consumed;
				assert!(p + len <= pl.len(), "OBU element length within payload");
				tu.extend_from_slice(&pl[p..p + len]);
				p += len;
			}
		}
		tu
	}

	#[test]
	fn av1_small_tu_single_packet() {
		// A small TU (≤ MTU) → exactly one packet, marker set; reassembles byte-for-byte.
		let tu = av1_tu(40);
		let pkts = roundtrip_codec(&tu, 900, Codec::Av1);
		assert_eq!(pkts.len(), 1);
		let out = depacketize_av1(&pkts, 900);
		assert_eq!(out, tu, "single-packet AV1 TU round-trips exactly");
	}

	#[test]
	fn av1_large_tu_fragments_and_reassembles() {
		// A >MTU key-frame TU must fragment across several packets and reassemble to the exact TU
		// (the regression: the old NAL-splitting path emitted ZERO packets for an OBU stream).
		let tu = av1_tu(MTU * 3);
		let pkts = roundtrip_codec(&tu, 4242, Codec::Av1);
		assert!(pkts.len() > 3, "a >3·MTU TU must fragment into several packets");
		// Every packet carries a Z=0/Y=0/W=0/N=0 aggregation header (no Annex-B, no OBU parsing).
		for pkt in &pkts {
			assert_eq!(pkt[RTP_HEADER_LEN], 0x00, "AV1 aggregation header must be 0x00");
		}
		let out = depacketize_av1(&pkts, 4242);
		assert_eq!(out, tu, "fragmented AV1 TU reassembles byte-for-byte");
	}

	#[test]
	fn av1_empty_tu_sends_nothing() {
		// A degenerate empty TU emits no packets (mirrors the H.264/HEVC empty-input contract).
		let pkts = roundtrip_codec(&[], 1, Codec::Av1);
		assert!(pkts.is_empty());
	}

	#[test]
	fn egress_thread_delivers_identical_bytes() {
		// The decoupled `RtpEgress` (sender thread) must put the SAME bytes on the wire as a
		// direct `RtpSender` — the wire contract is unchanged, only WHERE the send runs moved.
		let recv = UdpSocket::bind("127.0.0.1:0").unwrap();
		recv.set_read_timeout(Some(std::time::Duration::from_secs(2)))
			.unwrap();
		let dest = recv.local_addr().unwrap();
		let au = annexb_nal(1, 2, 100); // single-NAL → exactly one packet, marker set
		let egress =
			RtpEgress::spawn(dest, Codec::H264, 60, Arc::new(AtomicU32::new(20_000))).unwrap();
		egress.send_access_unit(&au, 4242);
		let mut buf = [0u8; 2048];
		let m = recv.recv(&mut buf).unwrap(); // blocks until the sender thread delivers it
		let nals = depacketize(&[buf[..m].to_vec()], 4242);
		assert_eq!(nals.len(), 1);
		assert_eq!(nals[0], &au[4..]);
		drop(egress); // closes the mailbox + joins the sender thread (no orphan)
	}

	/// A lost sequence range maps back to exactly the access units that carried it, across
	/// the 16-bit wrap.
	#[test]
	fn sent_log_maps_a_lost_range_to_its_frames() {
		let log: SentLog = Arc::new(Mutex::new(VecDeque::new()));
		{
			let mut l = log.lock().unwrap();
			l.push_back((1000, 65530, 65534));
			l.push_back((2000, 65535, 3));
			l.push_back((3000, 4, 4));
		}
		assert_eq!(frames_in_seq_range(&log, 65533, 65533), vec![1000]);
		assert_eq!(frames_in_seq_range(&log, 65534, 0), vec![1000, 2000]);
		assert_eq!(frames_in_seq_range(&log, 2, 9), vec![2000, 3000]);
		assert!(frames_in_seq_range(&log, 5, 9).is_empty());
	}

	/// The sender logs each access unit's first/last sequence number.
	#[test]
	fn the_sender_logs_what_it_sent() {
		let rx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
		let mut tx = RtpSender::new_with_codec(rx.local_addr().unwrap(), Codec::H264).unwrap();
		let mut au = vec![0, 0, 0, 1, 0x65];
		au.extend(std::iter::repeat_n(7u8, 4000)); // fragments into several packets
		tx.send_access_unit(&au, 90).unwrap();
		tx.send_access_unit(&[0, 0, 0, 1, 0x41, 1, 2, 3], 180).unwrap();
		let l = tx.sent_log();
		let l = l.lock().unwrap();
		assert_eq!(l.len(), 2);
		let (ts0, a0, b0) = l[0];
		let (ts1, a1, b1) = l[1];
		assert_eq!((ts0, ts1), (90, 180));
		assert!(b0.wrapping_sub(a0) >= 2, "an IDR spans several packets");
		assert_eq!(a1, b0.wrapping_add(1));
		assert_eq!(a1, b1);
	}

	/// Drive a `SendPacer` like `pace_gate` does with a fake clock: one AU of `bytes` in
	/// MTU packets, arriving at `arrival`, sent from `*now` on. Returns each packet's send time
	/// (ms since `t0`); `*now` ends at the last send.
	fn pace_au(
		p: &mut SendPacer,
		t0: Instant,
		now: &mut Instant,
		arrival: Instant,
		bytes: usize,
		target_kbps: u32,
		fps: u32,
	) -> Vec<f64> {
		if *now < arrival {
			*now = arrival;
		}
		p.begin_au(bytes as f64, target_kbps, fps, arrival, *now);
		let mut out = Vec::new();
		let mut left = bytes;
		while left > 0 {
			let c = left.min(MTU);
			let w = p.wait(c as f64, *now);
			*now += w;
			p.take(c as f64, *now);
			out.push(now.duration_since(t0).as_secs_f64() * 1000.0);
			left -= c;
		}
		out
	}

	#[test]
	fn pacer_spreads_a_40kb_keyframe_at_3_mbit() {
		let t0 = Instant::now();
		let mut now = t0;
		let mut p = SendPacer::new();
		let sent = pace_au(&mut p, t0, &mut now, t0, 40_000, 3000, 60);
		let spread = sent.last().unwrap() - sent[0];
		// The 16 ms budget (12 KB at 2 x 3 Mbit) leaves at once; the rest drips so the AU is
		// out within 1.5 frame intervals (25 ms at 60 fps) — not one line-rate burst.
		assert!(spread >= 15.0, "keyframe spread {spread:.1} ms");
		assert!(spread <= 1.5 * 1000.0 / 60.0 + 1.0, "keyframe spread {spread:.1} ms");
		let burst = sent.iter().filter(|t| **t == sent[0]).count();
		assert!(burst * MTU <= 12_000 + 1125 + MTU, "initial burst {burst} packets");
		// Packet-granular after the burst: no 2 ms span carries more than the drain rate
		// (~1.1 KB/ms here) allows plus one packet.
		for w in 0..25 {
			let lo = sent[0] + 0.5 + w as f64;
			let n = sent.iter().filter(|t| **t >= lo && **t < lo + 2.0).count();
			assert!(n <= 3, "{n} packets in the 2 ms from {lo:.1} ms");
		}
	}

	#[test]
	fn pacer_adds_no_delay_on_a_50_mbit_lan() {
		let t0 = Instant::now();
		let mut now = t0;
		let mut p = SendPacer::new();
		let interval = Duration::from_micros(8333); // 120 fps, ~52 KB a frame at 50 Mbit
		let mut worst = 0.0f64;
		for i in 0..360u32 {
			let arrival = t0 + interval * i;
			let size = 52_000 * (60 + (i * 37 % 80)) as usize / 100; // ±40 %
			let sent = pace_au(&mut p, t0, &mut now, arrival, size, 50_000, 120);
			let added = sent.last().unwrap() - arrival.duration_since(t0).as_secs_f64() * 1000.0;
			worst = worst.max(added);
		}
		assert!(worst <= 2.0, "worst added delay {worst:.2} ms");
	}

	#[test]
	fn pacer_keeps_back_to_back_keyframes_from_piling_up() {
		// An encoder emitting 40 KB every frame on a 3 Mbit target (a recovery storm): every AU
		// must still be out within ~1.5 intervals of its arrival, frame after frame.
		let t0 = Instant::now();
		let mut now = t0;
		let mut p = SendPacer::new();
		let interval = Duration::from_micros(16_667);
		for i in 0..30u32 {
			let arrival = t0 + interval * i;
			let sent = pace_au(&mut p, t0, &mut now, arrival, 40_000, 3000, 60);
			let lag = sent.last().unwrap() - arrival.duration_since(t0).as_secs_f64() * 1000.0;
			assert!(lag <= 1.5 * 16.667 + 1.5, "AU {i} out {lag:.1} ms after arrival");
		}
	}

	#[test]
	fn pacing_is_off_toward_the_loopback_intake() {
		// The media-over-session forwarder paces the wire; the loopback hop must not add a
		// second queue (unless PULSAR_RTP_PACE forces it).
		if std::env::var("PULSAR_RTP_PACE").is_ok() {
			return;
		}
		let recv = UdpSocket::bind("127.0.0.1:0").unwrap();
		// The unpaced burst must fit the receive buffer (Windows defaults to 64 KiB).
		let _ = socket2::SockRef::from(&recv).set_recv_buffer_size(8 * 1024 * 1024);
		recv.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
		let egress = RtpEgress::spawn(
			recv.local_addr().unwrap(),
			Codec::H264,
			60,
			Arc::new(AtomicU32::new(300)),
		)
		.unwrap();
		// 200 KB at a 300 kbit target would take ~0.8 s paced at the 2 Mbit floor.
		let au = annexb_nal(5, 3, 200_000);
		let t = Instant::now();
		egress.send_access_unit(&au, 1);
		let mut buf = [0u8; 2048];
		let mut n = 0;
		while let Ok(m) = recv.recv(&mut buf) {
			n += 1;
			if buf[1] & 0x80 != 0 && m > 0 {
				break;
			}
		}
		assert!(n > 100);
		assert!(t.elapsed() < Duration::from_millis(300), "loopback AU paced: {:?}", t.elapsed());
	}
}
