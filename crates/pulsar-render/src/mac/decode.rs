//! VideoToolbox decode: one access unit from the depacketizer (`stream/rtp.rs`) in, IOSurface-
//! backed `CVPixelBuffer`s (NV12 / P010, Metal-compatible) out — the macOS analogue of the
//! Windows Media Foundation decoder (`win/decode.rs`). H.264 and HEVC everywhere VideoToolbox
//! has them (hardware, or Apple's software decoder), AV1 where the Mac decodes it in hardware
//! (Apple M3 and later; `VTIsHardwareDecodeSupported`).
//!
//! The session is real-time (`kVTDecompressionPropertyKey_RealTime`) and driven
//! synchronously (no `EnableAsynchronousDecompression` flag): the output callback has normally
//! run when `DecodeFrame` returns, so the render loop gets the picture of an AU right away —
//! low-latency streams carry no B-frames, there is no reordering to wait for.

use std::ffi::c_void;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;

use super::bitstream;
use super::ffi::*;
use super::view::Matrix;
use crate::stream::{AccessUnit, Codec};

/// One decoded picture: a retained `CVPixelBuffer`.
#[derive(Clone)]
pub struct Frame {
	pub pb: Cf,
}

impl Frame {
	pub fn ptr(&self) -> CVPixelBufferRef {
		self.pb.ptr() as CVPixelBufferRef
	}
	pub fn size(&self) -> (u32, u32) {
		// SAFETY: a live pixel buffer.
		unsafe { (CVPixelBufferGetWidth(self.ptr()) as u32, CVPixelBufferGetHeight(self.ptr()) as u32) }
	}
	pub fn pixel_format(&self) -> u32 {
		// SAFETY: a live pixel buffer.
		unsafe { CVPixelBufferGetPixelFormatType(self.ptr()) }
	}
	/// The colour matrix the decoder attached (from the stream's VUI / colour config); BT.709
	/// when it says nothing — what Pulsar hosts encode.
	pub fn matrix(&self) -> Matrix {
		// SAFETY: a live buffer; the attachment is a +0 CFString (or null) we only compare.
		unsafe {
			let m = CVBufferGetAttachment(self.ptr(), kCVImageBufferYCbCrMatrixKey, std::ptr::null_mut());
			if m.is_null() {
				Matrix::Bt709
			} else if CFEqual(m, kCVImageBufferYCbCrMatrix_ITU_R_601_4) != 0 {
				Matrix::Bt601
			} else if CFEqual(m, kCVImageBufferYCbCrMatrix_ITU_R_2020) != 0 {
				Matrix::Bt2020
			} else {
				Matrix::Bt709
			}
		}
	}
}

/// What the output callback writes into (boxed: its address is the callback's refcon).
struct Shared {
	out: Mutex<Vec<Frame>>,
	/// The last non-zero status the callback reported (0 = none since the last take).
	err: AtomicI32,
}

unsafe extern "C" fn on_output(
	refcon: *mut c_void,
	_src: *mut c_void,
	status: OSStatus,
	info: u32,
	image: CVImageBufferRef,
	_pts: CMTime,
	_dur: CMTime,
) {
	// SAFETY: the refcon is the `Shared` the owning `Decoder` keeps alive (boxed) until it
	// invalidated the session, which waits for every callback.
	let sh = &*(refcon as *const Shared);
	if status != 0 {
		sh.err.store(status, Ordering::Relaxed);
		return;
	}
	if image.is_null() || info & kVTDecodeInfo_FrameDropped != 0 {
		return;
	}
	if let Some(pb) = Cf::retain(image as CFTypeRef) {
		if let Ok(mut q) = sh.out.lock() {
			q.push(Frame { pb });
		}
	}
}

/// `VTRegisterSupplementalVideoDecoderIfAvailable` (macOS 11+): makes the AV1 decoder
/// visible to VideoToolbox. Looked up at run time so the renderer still starts on older macOS.
fn register_av1() {
	static ONCE: std::sync::Once = std::sync::Once::new();
	ONCE.call_once(|| {
		extern "C" {
			fn dlsym(handle: *mut c_void, name: *const u8) -> *mut c_void;
		}
		const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;
		// SAFETY: a NUL-terminated name; the symbol, if present, has this signature.
		unsafe {
			let f = dlsym(RTLD_DEFAULT, b"VTRegisterSupplementalVideoDecoderIfAvailable\0".as_ptr());
			if !f.is_null() {
				let f: unsafe extern "C" fn(u32) = std::mem::transmute(f);
				f(kCMVideoCodecType_AV1);
			}
		}
	});
}

fn codec_type(c: Codec) -> u32 {
	match c {
		Codec::H264 => kCMVideoCodecType_H264,
		Codec::H265 => kCMVideoCodecType_HEVC,
		Codec::Av1 => kCMVideoCodecType_AV1,
	}
}

pub struct Decoder {
	codec: Codec,
	shared: Box<Shared>,
	format: Option<Cf>,
	session: VTDecompressionSessionRef,
	vps: Option<Vec<u8>>,
	sps: Option<Vec<u8>>,
	pps: Option<Vec<u8>>,
	av1_seq: Option<Vec<u8>>,
	/// The decoder in use runs in hardware (`None` until a session exists).
	pub hw: Option<bool>,
	/// A session failure the caller reports once (the session is rebuilt on the next AU from the
	/// kept parameter sets, keyframe or not — see `bitstream::needs_session`).
	pub failed: bool,
}

impl Drop for Decoder {
	fn drop(&mut self) {
		self.drop_session();
	}
}

impl Decoder {
	pub fn new(codec: Codec) -> Self {
		if codec == Codec::Av1 {
			register_av1();
		}
		Self {
			codec,
			shared: Box::new(Shared { out: Mutex::new(Vec::new()), err: AtomicI32::new(0) }),
			format: None,
			session: std::ptr::null_mut(),
			vps: None,
			sps: None,
			pps: None,
			av1_seq: None,
			hw: None,
			failed: false,
		}
	}

	fn drop_session(&mut self) {
		if !self.session.is_null() {
			// SAFETY: our session; Invalidate waits for pending callbacks.
			unsafe {
				VTDecompressionSessionWaitForAsynchronousFrames(self.session);
				VTDecompressionSessionInvalidate(self.session);
				CFRelease(self.session as CFTypeRef);
			}
			self.session = std::ptr::null_mut();
		}
	}

	/// A new format description from the parameter sets / sequence header seen so far.
	fn make_format(&self) -> Option<Cf> {
		let mut out: CMFormatDescriptionRef = std::ptr::null();
		// SAFETY: the pointer/size arrays describe live byte slices for the duration of the call.
		let st = unsafe {
			match self.codec {
				Codec::H264 => {
					let (sps, pps) = (self.sps.as_ref()?, self.pps.as_ref()?);
					let ptrs = [sps.as_ptr(), pps.as_ptr()];
					let sizes = [sps.len(), pps.len()];
					CMVideoFormatDescriptionCreateFromH264ParameterSets(
						kCFAllocatorDefault,
						2,
						ptrs.as_ptr(),
						sizes.as_ptr(),
						4,
						&mut out,
					)
				}
				Codec::H265 => {
					let (vps, sps, pps) = (self.vps.as_ref()?, self.sps.as_ref()?, self.pps.as_ref()?);
					let ptrs = [vps.as_ptr(), sps.as_ptr(), pps.as_ptr()];
					let sizes = [vps.len(), sps.len(), pps.len()];
					CMVideoFormatDescriptionCreateFromHEVCParameterSets(
						kCFAllocatorDefault,
						3,
						ptrs.as_ptr(),
						sizes.as_ptr(),
						4,
						std::ptr::null(),
						&mut out,
					)
				}
				Codec::Av1 => {
					let obu = self.av1_seq.as_ref()?;
					let (_, hlen) = obu_payload(obu)?;
					let seq = bitstream::parse_av1_seq(&obu[hlen..])?;
					let av1c = cf_data(&bitstream::av1c(&seq, obu));
					let key = cf_str("av1C");
					let atoms = cf_dict(&[(key.ptr(), av1c.ptr())]);
					let ext = cf_dict(&[(kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms, atoms.ptr())]);
					CMVideoFormatDescriptionCreate(
						kCFAllocatorDefault,
						kCMVideoCodecType_AV1,
						seq.max_width as i32,
						seq.max_height as i32,
						ext.ptr(),
						&mut out,
					)
				}
			}
		};
		if st != 0 {
			eprintln!("pulsar-render(mac): format description from the parameter sets failed ({st})");
			return None;
		}
		Cf::owned(out)
	}

	fn make_session(&mut self) -> Result<(), String> {
		let Some(format) = self.format.as_ref() else { return Err("no format".into()) };
		// Any of the 4:2:0 bi-planar formats, Metal-compatible, IOSurface-backed (zero-copy into
		// the CVMetalTextureCache). 8-bit first: a 10-bit stream still comes out as P010.
		let fmts: Vec<Cf> = [PIX_420V, PIX_420F, PIX_X420, PIX_XF20].iter().map(|f| cf_i32(*f as i32)).collect();
		let fmt_arr = cf_array(&fmts.iter().collect::<Vec<_>>());
		let iosurf = cf_dict(&[]);
		// SAFETY: the statics are framework CFStrings; values are live CF objects.
		let attrs = unsafe {
			cf_dict(&[
				(kCVPixelBufferPixelFormatTypeKey, fmt_arr.ptr()),
				(kCVPixelBufferMetalCompatibilityKey, kCFBooleanTrue),
				(kCVPixelBufferIOSurfacePropertiesKey, iosurf.ptr()),
			])
		};
		// SAFETY: as above.
		let spec = unsafe { cf_dict(&[(kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder, kCFBooleanTrue)]) };
		let rec = VTDecompressionOutputCallbackRecord {
			callback: Some(on_output),
			refcon: &*self.shared as *const Shared as *mut c_void,
		};
		let mut s: VTDecompressionSessionRef = std::ptr::null_mut();
		// SAFETY: valid format/dicts; the callback record is copied by VT; `shared` outlives the
		// session (dropped after it in `drop_session`).
		let st = unsafe {
			VTDecompressionSessionCreate(kCFAllocatorDefault, format.ptr(), spec.ptr(), attrs.ptr(), &rec, &mut s)
		};
		if st != 0 || s.is_null() {
			return Err(format!("VTDecompressionSessionCreate failed ({st})"));
		}
		self.session = s;
		// SAFETY: our session; standard keys + CFBoolean values.
		unsafe {
			VTSessionSetProperty(s as CFTypeRef, kVTDecompressionPropertyKey_RealTime, kCFBooleanTrue);
			VTSessionSetProperty(s as CFTypeRef, kVTDecompressionPropertyKey_MaximizePowerEfficiency, kCFBooleanFalse);
			let mut v: CFTypeRef = std::ptr::null();
			let hw = if VTSessionCopyProperty(
				s as CFTypeRef,
				kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder,
				kCFAllocatorDefault,
				&mut v as *mut CFTypeRef as *mut c_void,
			) == 0
			{
				let r = cf_bool(v);
				if !v.is_null() {
					CFRelease(v);
				}
				r
			} else {
				None
			};
			self.hw = Some(hw.unwrap_or(false));
		}
		Ok(())
	}

	/// Switch to a new format description: keep the session if it can take it, else rebuild.
	fn apply_format(&mut self, f: Cf) -> Result<(), String> {
		// SAFETY: live session + format.
		let keep = !self.session.is_null() && unsafe { VTDecompressionSessionCanAcceptFormatDescription(self.session, f.ptr()) } != 0;
		self.format = Some(f);
		if !keep {
			self.drop_session();
			self.make_session()?;
		}
		Ok(())
	}

	/// Decode one AU. Returns the pictures that came out (normally one). `Err` = this AU could
	/// not be decoded; the caller asks for a keyframe.
	pub fn decode(&mut self, au: &AccessUnit) -> Result<Vec<Frame>, String> {
		let sample = match self.codec {
			Codec::H264 | Codec::H265 => {
				let s = bitstream::split_nal_au(self.codec, &au.data);
				let mut changed = false;
				for (slot, new) in [(&mut self.vps, s.vps), (&mut self.sps, s.sps), (&mut self.pps, s.pps)] {
					if let Some(n) = new {
						if slot.as_ref() != Some(&n) {
							*slot = Some(n);
							changed = true;
						}
					}
				}
				let have_params = self.sps.is_some() && self.pps.is_some() && (self.codec != Codec::H265 || self.vps.is_some());
				if bitstream::needs_session(changed, !self.session.is_null(), have_params) {
					if let Some(f) = self.make_format() {
						self.apply_format(f)?;
					}
				}
				s.sample
			}
			Codec::Av1 => {
				let (sample, seq) = bitstream::av1_sample(&au.data);
				let changed = seq.as_ref().is_some_and(|s| self.av1_seq.as_ref() != Some(s));
				if let Some(seq) = seq {
					self.av1_seq = Some(seq);
				}
				if bitstream::needs_session(changed, !self.session.is_null(), self.av1_seq.is_some()) {
					if let Some(f) = self.make_format() {
						self.apply_format(f)?;
					}
				}
				sample
			}
		};
		if self.session.is_null() {
			return Err("no decoder session yet (waiting for the parameter sets)".into());
		}
		if sample.is_empty() {
			return Ok(Vec::new());
		}
		let Some(format) = self.format.as_ref() else { return Err("no format".into()) };
		// SAFETY: block buffer allocated by CM (null memory + AssureMemoryNow) and filled with a
		// copy of `sample`; the sample buffer retains it; both released at the end.
		unsafe {
			let mut bb: CMBlockBufferRef = std::ptr::null_mut();
			let st = CMBlockBufferCreateWithMemoryBlock(
				kCFAllocatorDefault,
				std::ptr::null_mut(),
				sample.len(),
				kCFAllocatorDefault,
				std::ptr::null(),
				0,
				sample.len(),
				kCMBlockBufferAssureMemoryNowFlag,
				&mut bb,
			);
			if st != 0 || bb.is_null() {
				return Err(format!("CMBlockBufferCreate failed ({st})"));
			}
			let bb = Cf(bb as CFTypeRef);
			CMBlockBufferReplaceDataBytes(sample.as_ptr() as *const c_void, bb.ptr() as CMBlockBufferRef, 0, sample.len());
			let timing = CMSampleTimingInfo {
				duration: CMTime::default(),
				presentationTimeStamp: CMTime {
					value: au.pts_90k as i64,
					timescale: 90_000,
					flags: kCMTimeFlags_Valid,
					epoch: 0,
				},
				decodeTimeStamp: CMTime::default(),
			};
			let size = sample.len();
			let mut sb: CMSampleBufferRef = std::ptr::null_mut();
			let st = CMSampleBufferCreateReady(
				kCFAllocatorDefault,
				bb.ptr() as CMBlockBufferRef,
				format.ptr(),
				1,
				1,
				&timing,
				1,
				&size,
				&mut sb,
			);
			if st != 0 || sb.is_null() {
				return Err(format!("CMSampleBufferCreateReady failed ({st})"));
			}
			let sb = Cf(sb as CFTypeRef);
			let mut info = 0u32;
			let st = VTDecompressionSessionDecodeFrame(self.session, sb.ptr() as CMSampleBufferRef, 0, std::ptr::null_mut(), &mut info);
			// A synchronous decode has normally called back already; make sure the picture of THIS
			// AU is out before returning (a decoder that still queues internally would add a
			// frame of latency otherwise).
			if st == 0 {
				VTDecompressionSessionWaitForAsynchronousFrames(self.session);
			}
			let cb_err = self.shared.err.swap(0, Ordering::Relaxed);
			let st = if st != 0 { st } else { cb_err };
			if st == kVTInvalidSessionErr || st == kVTVideoDecoderMalfunctionErr {
				// The session died (sleep / GPU change / decoder reset): rebuilt on the next AU from
				// the parameter sets we keep (no keyframe needed: intra refresh / RFI heal the
				// picture), while the caller still asks for an IDR.
				self.drop_session();
				self.failed = true;
				return Err(format!("decoder session lost ({st})"));
			}
			if st != 0 {
				return Err(format!("VTDecompressionSessionDecodeFrame failed ({st})"));
			}
		}
		Ok(std::mem::take(&mut *self.shared.out.lock().unwrap()))
	}
}

/// (payload length, header length incl. the size field) of a sized OBU.
fn obu_payload(obu: &[u8]) -> Option<(usize, usize)> {
	let b = *obu.first()?;
	let mut h = if b & 0x04 != 0 { 2 } else { 1 };
	if b & 0x02 != 0 {
		let (v, n) = bitstream::leb128(obu.get(h..)?)?;
		h += n;
		return Some((v as usize, h));
	}
	Some((obu.len() - h, h))
}

/// Headless capability probe (main.rs `--probe`), the same JSON as the Windows / Linux
/// probes: H.264 / HEVC decode everywhere (hardware or Apple's software decoder), AV1 only
/// where VideoToolbox decodes it in hardware.
pub fn probe_json() -> String {
	register_av1();
	let mut entries = Vec::new();
	for (name, c) in [("h264", Codec::H264), ("h265", Codec::H265), ("av1", Codec::Av1)] {
		// SAFETY: plain query.
		let hw = unsafe { VTIsHardwareDecodeSupported(codec_type(c)) } != 0;
		let ok = hw || c != Codec::Av1;
		entries.push(format!(
			"{{\"codec\":\"{name}\",\"ok\":{ok},\"decoder\":\"videotoolbox\",\"hw\":{hw},\"tier\":\"{}\",\"incompatible_with\":[]}}",
			if hw { "hwaccel" } else { "software" }
		));
	}
	format!("[{}]", entries.join(","))
}
