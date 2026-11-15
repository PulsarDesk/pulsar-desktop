//! Lists this machine's hardware Media Foundation encoders and the `ICodecAPI` knobs each one
//! takes (live bitrate, LTR, low latency, …): `cargo run -p pulsar-capture --example mfprobe`.

#[cfg(windows)]
fn main() {
	print!("{}", pulsar_capture::mf_report());
	println!("mf_codecs (non-NVIDIA display GPU): {:?}", pulsar_capture::mf_codecs());
}

#[cfg(not(windows))]
fn main() {
	eprintln!("mfprobe: Windows only");
}
