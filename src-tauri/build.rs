fn main() {
	// Windows: the app manifest (Common-Controls v6) is embedded by the LINKER below instead of
	// tauri-build's resource file, so the test executables get it too (see there).
	let attrs = tauri_build::Attributes::new()
		.windows_attributes(tauri_build::WindowsAttributes::new_without_app_manifest());
	if let Err(e) = tauri_build::try_build(attrs) {
		panic!("tauri-build failed: {e:#}");
	}
	// NOTE: do NOT link -lEGL/-lGL into THIS binary — doing so perturbs WebKitGTK's own
	// GL/epoxy loading and wedges its compositor (blank webview). The single-surface
	// renderer resolves GL getProcAddress from libepoxy at runtime instead (native_view.rs).

	// On Linux, compile the native zero-copy video sink (`pulsar-vidsink`) as a SEPARATE
	// executable placed next to the app binary. It replaces mpv on the Pi (rkmpp →
	// DRM_PRIME → EGL, no GPU→CPU download). Linking EGL/GLES2/X11 into IT is fine — it's
	// its own process, so the WebKitGTK caveat above doesn't apply. If the toolchain or
	// ffmpeg dev libs are missing we just warn and skip (the client falls back to mpv).
	#[cfg(target_os = "linux")]
	build_vidsink();

	// macOS: pulsar-core's per-window capture uses ScreenCaptureKit (macOS 12.3+), and
	// objc2-screen-capture-kit links that framework STRONGLY. The app's minimum is older
	// (Tauri's default 10.13), where dyld would refuse to start the whole app over a
	// missing framework. Weak-link it instead: the framework then loads only where it
	// exists, and `window_capture::backend()` checks for the classes at runtime (no
	// per-window source on older macOS). ld64 makes a dylib weak as soon as ONE of its
	// load options says so, so this wins over the crate's own `-framework`. Target cfg via
	// the env var: `#[cfg]` here would be the BUILD host's OS. Untested on macOS hardware.
	if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
		println!("cargo:rustc-link-arg=-Wl,-weak_framework,ScreenCaptureKit");
	}

	// Windows: the app imports TaskDialogIndirect (dialogs), which only the Common-Controls v6
	// comctl32 exports, and v6 is bound only through an application manifest. tauri-build
	// embeds its manifest as a resource of the APP only; the test harness got none, so
	// `cargo test -p pulsar-tauri` died at load with STATUS_ENTRYPOINT_NOT_FOUND. The same
	// manifest (tauri-build's default, windows-app-manifest.xml) is now embedded by link.exe
	// into every executable of this package, the app and its tests alike.
	if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
		let manifest = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("windows-app-manifest.xml");
		println!("cargo:rerun-if-changed={}", manifest.display());
		println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
		println!("cargo:rustc-link-arg=/MANIFESTINPUT:{}", manifest.display());
	}
}

#[cfg(target_os = "linux")]
fn build_vidsink() {
	use std::path::Path;
	use std::process::Command;
	let src = "../scripts/pulsar-vidsink.c";
	println!("cargo:rerun-if-changed={src}");
	if !Path::new(src).exists() {
		return;
	}
	// Place it next to the app exe (target/<profile>/pulsar-vidsink) so the runtime resolver
	// (process::vidsink_bin → bundled_bin → next-to-exe) finds it. OUT_DIR is
	// target/<profile>/build/pulsar-tauri-<hash>/out, so the profile dir is 3 levels up.
	let out_dir = std::env::var("OUT_DIR").unwrap();
	let Some(profile_dir) = Path::new(&out_dir).ancestors().nth(3) else {
		return;
	};
	let out_bin = profile_dir.join("pulsar-vidsink");
	let pc = match Command::new("pkg-config")
		.args([
			"--cflags",
			"--libs",
			"libavformat",
			"libavcodec",
			"libavutil",
		])
		.output()
	{
		Ok(o) if o.status.success() => o.stdout,
		_ => {
			println!("cargo:warning=pulsar-vidsink: ffmpeg dev libs/pkg-config missing — skipping (mpv fallback)");
			return;
		}
	};
	let flags = String::from_utf8_lossy(&pc);
	let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
	let mut cmd = Command::new(&cc);
	cmd.arg("-O2").arg("-o").arg(&out_bin).arg(src);
	cmd.args(flags.split_whitespace());
	cmd.args(["-lEGL", "-lGLESv2", "-lX11", "-lpthread"]);
	match cmd.status() {
		Ok(s) if s.success() => {
			println!("cargo:warning=pulsar-vidsink: built {}", out_bin.display())
		}
		_ => println!("cargo:warning=pulsar-vidsink: compile failed — mpv fallback will be used"),
	}
}
