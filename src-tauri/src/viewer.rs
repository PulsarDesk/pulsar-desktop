//! Local media intake: the UDP ports the host's RTP reaches on this machine.
//!
//! Audio is broadcast in-process to the native player ([`crate::native_audio`]); nothing
//! about it touches the webview. Video keeps its older UDP→WebSocket relay (from the removed
//! in-webview WebCodecs viewer).

use std::sync::Arc;

use futures_util::SinkExt;
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

/// A running relay. Holds the task handles so it can be torn down on stop.
pub struct Viewer {
	/// UDP port the host should stream **video** RTP to.
	pub media_port: u16,
	/// Loopback WebSocket port the webview connects to for **video**.
	pub ws_port: u16,
	/// UDP port the host should stream **audio** (Opus RTP) to.
	pub audio_port: u16,
	/// Broadcast of received **audio** RTP datagrams (the native player subscribes).
	audio_tx: broadcast::Sender<Vec<u8>>,
	tasks: Vec<JoinHandle<()>>,
}

impl Viewer {
	pub fn stop(self) {
		for t in self.tasks {
			t.abort();
		}
	}

	/// Every audio RTP datagram from now on. Closes when the viewer stops.
	pub fn audio_packets(&self) -> broadcast::Receiver<Vec<u8>> {
		self.audio_tx.subscribe()
	}
}

/// Start the intake: video (UDP + its WebSocket relay) and audio (UDP, broadcast only).
///
/// `loopback_only`: media-over-session is negotiated, so the RTP arrives via the
/// session demuxer ON THIS MACHINE — bind the intake sockets to 127.0.0.1 (nothing
/// external may feed them; zero externally-reachable media ports).
pub async fn start(loopback_only: bool) -> std::io::Result<Viewer> {
	let mut tasks = Vec::new();
	let (media_port, ws_port, _video_tx) = relay(&mut tasks, loopback_only, true).await?;
	let (audio_port, _, audio_tx) = relay(&mut tasks, loopback_only, false).await?;
	Ok(Viewer {
		media_port,
		ws_port,
		audio_port,
		audio_tx,
		tasks,
	})
}

/// Bind one intake (an ephemeral UDP port for the host's RTP, re-broadcast in-process) and,
/// with `ws`, a loopback WebSocket that relays it too. Spawns the pump tasks onto `tasks` and
/// returns `(udp_port, ws_port, broadcast)`; `ws_port` is 0 without `ws`.
async fn relay(
	tasks: &mut Vec<JoinHandle<()>>,
	loopback_only: bool,
	ws: bool,
) -> std::io::Result<(u16, u16, broadcast::Sender<Vec<u8>>)> {
	// Legacy direct flows need `0.0.0.0` so the host can reach us over LAN; under
	// media-over-session the feeder is the local demuxer → loopback-only. Bind via
	// socket2 to raise SO_RCVBUF: a 40 Mbps stream bursts hundreds of ~1400 B
	// datagrams per keyframe in a few ms, overflowing the default kernel recv buffer
	// and silently dropping packets — and dropped RTP corrupts H.264 until the next
	// keyframe. 8 MiB absorbs keyframe bursts + scheduling jitter.
	let sock = socket2::Socket::new(
		socket2::Domain::IPV4,
		socket2::Type::DGRAM,
		Some(socket2::Protocol::UDP),
	)?;
	sock.set_recv_buffer_size(8 * 1024 * 1024)?;
	sock.set_nonblocking(true)?; // required by UdpSocket::from_std
	let bind_ip = if loopback_only {
		"127.0.0.1:0"
	} else {
		"0.0.0.0:0"
	};
	sock.bind(&bind_ip.parse::<std::net::SocketAddr>().unwrap().into())?;
	let udp = Arc::new(UdpSocket::from_std(sock.into())?);
	let media_port = udp.local_addr()?.port();

	let (tx, _rx) = broadcast::channel::<Vec<u8>>(8192);

	// UDP datagrams → broadcast.
	let udp_rx = udp.clone();
	let tx_udp = tx.clone();
	let t_udp = tokio::spawn(async move {
		let mut buf = vec![0u8; 65_536];
		loop {
			match udp_rx.recv(&mut buf).await {
				Ok(n) => {
					let _ = tx_udp.send(buf[..n].to_vec());
				}
				Err(_) => break,
			}
		}
	});

	tasks.push(t_udp);
	if !ws {
		return Ok((media_port, 0, tx));
	}
	let ws_listener = TcpListener::bind("127.0.0.1:0").await?;
	let ws_port = ws_listener.local_addr()?.port();
	// Accept webview WebSocket connections; each forwards the broadcast.
	let tx_ws = tx.clone();
	let t_ws = tokio::spawn(async move {
		while let Ok((stream, _)) = ws_listener.accept().await {
			let mut rx = tx_ws.subscribe();
			tokio::spawn(async move {
				let ws = match tokio_tungstenite::accept_async(stream).await {
					Ok(ws) => ws,
					Err(_) => return,
				};
				let (mut sink, _read) = futures_util::StreamExt::split(ws);
				loop {
					match rx.recv().await {
						Ok(pkt) => {
							if sink.send(Message::Binary(pkt.into())).await.is_err() {
								break;
							}
						}
						// Drop backlog rather than disconnect if the webview lags — but log
						// it: lagged drops lose RTP packets (the client resyncs on the next
						// keyframe), so this is the signal the webview is the bottleneck.
						Err(broadcast::error::RecvError::Lagged(n)) => {
							tracing::warn!("viewer ws lagged — dropped {n} datagrams");
							continue;
						}
						Err(broadcast::error::RecvError::Closed) => break,
					}
				}
			});
		}
	});

	tasks.push(t_ws);
	Ok((media_port, ws_port, tx))
}

#[cfg(test)]
mod tests {
	use super::*;
	use futures_util::StreamExt;
	use std::time::Duration;

	#[tokio::test]
	async fn audio_reaches_the_native_player_and_no_websocket() {
		let v = start(true).await.unwrap();
		let mut rx = v.audio_packets();
		let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
		sender
			.send_to(b"opus-rtp", ("127.0.0.1", v.audio_port))
			.await
			.unwrap();
		let got = tokio::time::timeout(Duration::from_secs(2), rx.recv())
			.await
			.expect("an audio datagram in time")
			.unwrap();
		assert_eq!(got, b"opus-rtp");
		v.stop();
		// Stopping the viewer closes the stream, which ends the player.
		assert!(matches!(
			tokio::time::timeout(Duration::from_secs(2), rx.recv()).await,
			Ok(Err(broadcast::error::RecvError::Closed))
		));
	}

	#[tokio::test]
	async fn forwards_udp_datagrams_to_the_websocket() {
		let v = start(false).await.unwrap();
		let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{}", v.ws_port))
			.await
			.unwrap();

		// Let the WS connection subscribe to the broadcast before sending.
		tokio::time::sleep(Duration::from_millis(150)).await;
		let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
		sender
			.send_to(b"rtp-payload", ("127.0.0.1", v.media_port))
			.await
			.unwrap();

		let msg = tokio::time::timeout(Duration::from_secs(2), ws.next())
			.await
			.expect("a forwarded message in time")
			.expect("stream open")
			.expect("valid frame");
		assert_eq!(msg.into_data(), &b"rtp-payload"[..]);
		v.stop();
	}
}
