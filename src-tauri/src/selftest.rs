//! `pulsar --selftest`: a headless check that this build's bundled media stack loads and runs
//! on the machine it is on — what CI runs against the AppImage on several distros, and what a
//! user can paste when a session stays black. No GUI, no Tauri runtime: everything is resolved
//! from the executable's location.
//!
//! Each line is `PASS|WARN|FAIL <check>: <detail>`; the exit code is 1 when a REQUIRED check
//! failed (WARN = optional hardware path unavailable, e.g. no VA-API driver in a container).

/// Run every check, print the report, return the process exit code.
pub fn run() -> i32 {
	#[cfg(target_os = "linux")]
	{
		linux::run()
	}
	#[cfg(not(target_os = "linux"))]
	{
		println!("PASS selftest: nothing to check on this platform");
		0
	}
}

#[cfg(target_os = "linux")]
mod linux {
	use pulsar_core::pipeline::gst::{encoder_fragment, GstEncoder};
	use pulsar_core::pipeline::VCodec;
	use std::path::PathBuf;
	use std::process::Command;

	struct Report {
		failed: bool,
	}

	impl Report {
		fn line(&mut self, required: bool, name: &str, res: Result<String, String>) {
			match res {
				Ok(d) => println!("PASS {name}: {d}"),
				Err(e) if required => {
					self.failed = true;
					println!("FAIL {name}: {e}");
				}
				Err(e) => println!("WARN {name}: {e}"),
			}
		}
	}

	/// The bundled resources dir: `usr/lib/Pulsar/resources` next to `usr/bin/pulsar` in the
	/// AppImage/.deb/.rpm layout, else next to the exe (dev builds).
	fn resources() -> Option<PathBuf> {
		let exe = std::env::current_exe().ok()?;
		let dir = exe.parent()?;
		[
			dir.join("../lib/Pulsar/resources"),
			dir.join("resources"),
			dir.to_path_buf(),
		]
		.into_iter()
		.find(|d| d.join("pulsar-render").is_file() || d.join("ffmpeg").is_file())
	}

	fn run_tool(mut cmd: Command) -> Result<String, String> {
		let out = cmd.output().map_err(|e| format!("spawn: {e}"))?;
		if !out.status.success() {
			let err = String::from_utf8_lossy(&out.stderr);
			let last = err
				.lines()
				.rev()
				.find(|l| !l.trim().is_empty())
				.unwrap_or("");
			return Err(format!("{} {last}", out.status));
		}
		Ok(String::from_utf8_lossy(&out.stdout).into_owned())
	}

	fn gst(enc: GstEncoder) -> Result<String, String> {
		let frag = encoder_fragment(enc, VCodec::H264, 4000, 30).ok_or("no H.264 element")?;
		pulsar_core::capture::self_check(&frag).map(|()| "encode ok".into())
	}

	pub fn run() -> i32 {
		let mut r = Report { failed: false };
		let res = resources();
		r.line(
			true,
			"resources",
			res.as_ref()
				.map(|d| d.display().to_string())
				.ok_or_else(|| "bundled resources dir not found".into()),
		);
		if let Some(res) = &res {
			let mut ff = Command::new(res.join("ffmpeg"));
			ff.args(["-hide_banner", "-version"]);
			r.line(
				true,
				"ffmpeg",
				run_tool(ff).map(|o| o.lines().next().unwrap_or("").to_string()),
			);
			// Same lib resolution as the real renderer (process.rs), so a missing/shadowed
			// libav shows up here instead of as a black viewer.
			let mut probe = Command::new(res.join("pulsar-render"));
			probe.arg("--probe");
			crate::process::apply_render_lib_env(&mut probe);
			r.line(
				true,
				"pulsar-render",
				run_tool(probe).and_then(|o| {
					let v: serde_json::Value = serde_json::from_str(o.trim())
						.map_err(|e| format!("bad probe output: {e}"))?;
					let decoders = v
						.as_array()
						.ok_or("probe output is not a list")?
						.iter()
						.map(|d| {
							format!(
								"{}={}({})",
								d["codec"].as_str().unwrap_or("?"),
								d["decoder"].as_str().unwrap_or("-"),
								d["tier"].as_str().unwrap_or("?")
							)
						})
						.collect::<Vec<_>>();
					Ok(decoders.join(" "))
				}),
			);
		}
		r.line(true, "gst x264 (Wayland capture)", gst(GstEncoder::X264));
		r.line(false, "gst vaapi", gst(GstEncoder::Vaapi));
		r.line(false, "gst va", gst(GstEncoder::Va));
		r.line(false, "gst nvenc", gst(GstEncoder::Nv));
		// What a Wayland host would actually advertise, and on which gst (an encoder the
		// bundled stack can't open may still work on the system's — process.rs falls back).
		let validated = crate::process::validated_gst_encoders();
		r.line(
			false,
			"gst encoders",
			if validated.is_empty() {
				Err("none (gst-launch-1.0 missing)".into())
			} else {
				Ok(validated
					.iter()
					.map(|(e, codecs)| {
						let on = |c: &VCodec| {
							if crate::process::gst_on_system(*e, *c) {
								"system-child"
							} else {
								"in-process"
							}
						};
						let codecs = codecs
							.iter()
							.map(|c| format!("{c:?}@{}", on(c)))
							.collect::<Vec<_>>()
							.join(",");
						format!("{}={codecs}", e.wire_id())
					})
					.collect::<Vec<_>>()
					.join(" "))
			},
		);
		// Whether main.rs::appimage_host_libva swapped in the host's libva (VA-API needs it).
		if std::env::var_os("APPDIR").is_some() {
			let preload = std::env::var("LD_PRELOAD").unwrap_or_default();
			r.line(
				false,
				"libva",
				match preload.split(':').find(|p| p.ends_with("/libva.so.2")) {
					Some(p) => Ok(format!("host {p}")),
					None => Err("bundled 2.14 (host has none, or PULSAR_HOST_LIBVA set) — no VA-API on newer drivers".into()),
				},
			);
		}
		i32::from(r.failed)
	}
}
