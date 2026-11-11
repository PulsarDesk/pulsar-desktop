//! `pulsar-sysagent` — Windows SYSTEM input agent for the Pulsar host.
//!
//! ```text
//! pulsar-sysagent service [--client-dir <dir>]… [--allow-medium]
//!     Run under the SCM as LocalSystem; keep one helper alive per logged-on session.
//! pulsar-sysagent helper  [--client-dir <dir>]… [--allow-medium]
//!     (spawned by the service with a SYSTEM token in a user session) serve the
//!     session's input pipe and inject on the current input desktop.
//! pulsar-sysagent install [--allow-any-path] [--client-dir <dir>]… [--allow-medium]
//! pulsar-sysagent uninstall | stop
//!     Admin-only service management, called by the installer.
//! ```
//!
//! Design: docs/agents/system-agent.md.
#![cfg_attr(not(windows), allow(dead_code))]

#[cfg(windows)]
mod helper;
#[cfg(windows)]
mod inject;
#[cfg(windows)]
mod install;
#[cfg(windows)]
mod log;
#[cfg(windows)]
mod service;

/// Options forwarded service → helper (and written into the service command line by `install`).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct AgentOpts {
	pub client_dirs: Vec<std::path::PathBuf>,
	pub allow_medium: bool,
	pub allow_any_path: bool,
}

impl AgentOpts {
	fn parse(args: &[String]) -> Result<Self, String> {
		let mut o = AgentOpts::default();
		let mut it = args.iter();
		while let Some(a) = it.next() {
			match a.as_str() {
				"--client-dir" => {
					let d = it.next().ok_or("--client-dir needs a directory")?;
					o.client_dirs.push(d.into());
				}
				"--allow-medium" => o.allow_medium = true,
				"--allow-any-path" => o.allow_any_path = true,
				other => return Err(format!("unknown argument {other:?}")),
			}
		}
		Ok(o)
	}

	/// Arguments to hand to the next process (never `--allow-any-path`, install-time only).
	pub fn forward(&self) -> Vec<String> {
		let mut v = Vec::new();
		for d in &self.client_dirs {
			v.push("--client-dir".into());
			v.push(d.to_string_lossy().into_owned());
		}
		if self.allow_medium {
			v.push("--allow-medium".into());
		}
		v
	}
}

fn usage() -> i32 {
	eprintln!(
		"pulsar-sysagent {} — Pulsar SYSTEM input agent (Windows)\n\
		 usage: pulsar-sysagent <service|helper|install|uninstall|stop> [options]",
		env!("CARGO_PKG_VERSION")
	);
	2
}

fn main() {
	let args: Vec<String> = std::env::args().skip(1).collect();
	let Some(mode) = args.first().cloned() else {
		std::process::exit(usage());
	};
	let opts = match AgentOpts::parse(&args[1..]) {
		Ok(o) => o,
		Err(e) => {
			eprintln!("pulsar-sysagent: {e}");
			std::process::exit(usage());
		}
	};
	std::process::exit(run(&mode, opts));
}

#[cfg(windows)]
fn run(mode: &str, opts: AgentOpts) -> i32 {
	match mode {
		"service" => service::run(opts),
		"helper" => helper::run(opts),
		"install" => install::install(&opts),
		"uninstall" => install::uninstall(),
		"stop" => install::stop(),
		"--version" | "-V" => {
			println!("{}", env!("CARGO_PKG_VERSION"));
			0
		}
		_ => usage(),
	}
}

#[cfg(not(windows))]
fn run(_mode: &str, _opts: AgentOpts) -> i32 {
	eprintln!("pulsar-sysagent is Windows-only");
	1
}

#[cfg(test)]
mod tests {
	use super::AgentOpts;

	fn s(v: &[&str]) -> Vec<String> {
		v.iter().map(|x| x.to_string()).collect()
	}

	#[test]
	fn parses_and_forwards_options() {
		let o = AgentOpts::parse(&s(&[
			"--client-dir",
			r"C:\dev\a b",
			"--allow-medium",
			"--allow-any-path",
		]))
		.unwrap();
		assert_eq!(o.client_dirs, vec![std::path::PathBuf::from(r"C:\dev\a b")]);
		assert!(o.allow_medium && o.allow_any_path);
		assert_eq!(
			o.forward(),
			s(&["--client-dir", r"C:\dev\a b", "--allow-medium"])
		);
		assert!(AgentOpts::parse(&s(&["--client-dir"])).is_err());
		assert!(AgentOpts::parse(&s(&["--bogus"])).is_err());
		assert_eq!(AgentOpts::parse(&[]).unwrap(), AgentOpts::default());
	}
}
