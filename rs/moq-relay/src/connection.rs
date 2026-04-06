use crate::{Auth, AuthParams, Cluster};

use axum::http;
use moq_native::Request;

/// An incoming connection that has not yet been authenticated.
///
/// Call [`run`](Self::run) to authenticate the request, wire up
/// publish/subscribe origins, and serve the session until it closes.
pub struct Connection {
	/// A numeric identifier for logging.
	pub id: u64,
	/// The raw QUIC/WebTransport request to accept or reject.
	pub request: Request,
	/// The cluster state used to resolve origins.
	pub cluster: Cluster,
	/// The authenticator used to verify credentials.
	pub auth: Auth,
	/// Send video datagrams to all other connections
	pub datagram_tx: tokio::sync::broadcast::Sender<bytes::Bytes>,
	/// Receive video datagrams from other connections
	pub datagram_rx: tokio::sync::broadcast::Receiver<bytes::Bytes>,
}

impl Connection {
	/// Authenticates and serves this connection until it closes.
	#[tracing::instrument("conn", skip_all, fields(id = self.id))]
	pub async fn run(self) -> anyhow::Result<()> {
		let params = match self.request.url() {
			Some(url) => AuthParams::from_url(url),
			None => AuthParams::default(),
		};

		// Verify the URL before accepting the connection.
		let token = match self.auth.verify(&params).await {
			Ok(token) => token,
			Err(err) => {
				let status: http::StatusCode = err.clone().into();
				let _ = self.request.close(status.as_u16()).await;
				return Err(err.into());
			}
		};

		let publish = self.cluster.publisher(&token);
		let subscribe = self.cluster.subscriber(&token);
		let registration = self.cluster.register(&token);
		let transport = self.request.transport();

		match (&publish, &subscribe) {
			(Some(publish), Some(subscribe)) => {
				tracing::info!(transport, root = %token.root, publish = %publish.allowed().map(|p| p.as_str()).collect::<Vec<_>>().join(","), subscribe = %subscribe.allowed().map(|p| p.as_str()).collect::<Vec<_>>().join(","), "session accepted");
			}
			(Some(publish), None) => {
				tracing::info!(transport, root = %token.root, publish = %publish.allowed().map(|p| p.as_str()).collect::<Vec<_>>().join(","), "publisher accepted");
			}
			(None, Some(subscribe)) => {
				tracing::info!(transport, root = %token.root, subscribe = %subscribe.allowed().map(|p| p.as_str()).collect::<Vec<_>>().join(","), "subscriber accepted")
			}
			_ => anyhow::bail!("invalid session; no allowed paths"),
		}

		// Accept the connection with datagram support for clock sync.
		// NOTE: subscribe and publish seem backwards because of how relays work.
		// We publish the tracks the client is allowed to subscribe to.
		// We subscribe to the tracks the client is allowed to publish.
		let (session, dg_handle) = self
			.request
			.with_publish(subscribe)
			.with_consume(publish)
			.ok_with_datagrams()
			.await?;

		// Spawn datagram handler (cloq clock sync + transport stats feedback + video datagram relay)
		if let Some(dg) = dg_handle {
			let conn_id = self.id;
			tokio::spawn(run_datagram_handler(conn_id, dg, self.datagram_tx, self.datagram_rx));
		}

		tracing::info!(version = %session.version(), transport, "negotiated");

		// Wait until the session is closed.
		// Keep registration alive so the cluster node stays announced.
		session.closed().await?;
		drop(registration);
		Ok(())
	}
}

/// Handles datagrams: clock heartbeat, transport stats, keyframe requests, video relay.
///
/// Relay clock heartbeat (relay → all clients, every 500ms):
///   [0x0A][relay_time_us:u64 BE]  (9 bytes)
///
/// Transport stats feedback (relay → client, every 500ms):
///   [0x03][recv_bytes:u64 BE][recv_packets:u64 BE][lost_packets:u64 BE]
///   [rtt_us:u64 BE][cwnd:u64 BE][timestamp_us:u64 BE]
///
/// Legacy clock sync (0x01/0x02) still handled for backwards compat.
async fn run_datagram_handler(
	conn_id: u64,
	dg: moq_native::DatagramHandle,
	datagram_tx: tokio::sync::broadcast::Sender<bytes::Bytes>,
	mut datagram_rx: tokio::sync::broadcast::Receiver<bytes::Bytes>,
) {
	use web_transport_trait::Stats;

	// Spawn stats feedback + bitrate controller + keyframe request sender
	let dg_stats = dg.clone();
	let stats_task = tokio::spawn(async move {
		use adamoq_bitrate::{AllocatorState, BitrateController, TransportStats as BStats};

		let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
		interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

		// Assume 2Mbps publisher target; will be updated when we learn the actual rate.
		let mut controller = BitrateController::new(2_000_000, std::time::Instant::now());
		let mut last_keyframe_request = std::time::Instant::now();

		// Track inbound throughput (peer → relay). This is the pipe that matters
		// for a publisher: how fast can they send us data.
		let mut prev_bytes_received: u64 = 0;
		let mut prev_time = std::time::Instant::now();

		loop {
			interval.tick().await;

			let stats = dg_stats.stats();
			let now_inst = std::time::Instant::now();
			let rtt_us = stats.rtt().map(|r| r.as_micros() as u64).unwrap_or(0);
			let recv = stats.packets_received().unwrap_or(0);
			let lost = stats.packets_lost().unwrap_or(0);
			let bytes_received = stats.bytes_received().unwrap_or(0);
			let send_rate = stats.estimated_send_rate().unwrap_or(0);

			// Inbound throughput: how fast peer is actually delivering to us.
			// For publisher connections, this IS the pipe we care about.
			let delta_bytes = bytes_received.saturating_sub(prev_bytes_received);
			let delta_s = now_inst.duration_since(prev_time).as_secs_f64().max(0.001);
			let inbound_throughput_bps = ((delta_bytes as f64 * 8.0) / delta_s) as u64;
			prev_bytes_received = bytes_received;
			prev_time = now_inst;

			// Send transport stats (0x03)
			let mut buf = bytes::BytesMut::with_capacity(49);
			buf.extend_from_slice(&[0x03]);
			buf.extend_from_slice(&bytes_received.to_be_bytes());
			buf.extend_from_slice(&recv.to_be_bytes());
			buf.extend_from_slice(&lost.to_be_bytes());
			buf.extend_from_slice(&rtt_us.to_be_bytes());
			buf.extend_from_slice(&send_rate.to_be_bytes());
			buf.extend_from_slice(&now_us().to_be_bytes());

			if dg_stats.send(buf.freeze()).is_err() {
				break;
			}

			// Send relay clock heartbeat (0x0A)
			let mut hb = bytes::BytesMut::with_capacity(9);
			hb.extend_from_slice(&[0x0A]);
			hb.extend_from_slice(&now_us().to_be_bytes());
			if dg_stats.send(hb.freeze()).is_err() {
				break;
			}

			// Run bitrate controller using INBOUND throughput (publisher→relay pipe).
			// This is the right signal for controlling the publisher's encode rate.
			let tstats = BStats {
				estimated_send_rate_bps: inbound_throughput_bps,
				packets_sent: recv,   // packets the peer sent us
				packets_lost: lost,
				bytes_sent: bytes_received,
				rtt_us,
			};
			if let Some(signal) = controller.update(tstats, std::time::Instant::now()) {
				// In Stable state, send ceiling=0 to mean "no limit, use your configured max".
				// Otherwise send the computed ceiling in kbps.
				let ceiling_kbps = if matches!(signal.state, AllocatorState::Stable) {
					0u32
				} else {
					(signal.ceiling_bps / 1000) as u32
				};
				let mut cc_buf = bytes::BytesMut::with_capacity(6);
				cc_buf.extend_from_slice(&[0x07]);
				cc_buf.extend_from_slice(&ceiling_kbps.to_be_bytes());
				cc_buf.extend_from_slice(&[signal.state.as_u8()]);
				let _ = dg_stats.send(cc_buf.freeze());

				// Request keyframe when entering Deficient state with significant loss.
				if matches!(signal.state, AllocatorState::Deficient)
					&& signal.estimate.loss_rate > 0.05
					&& last_keyframe_request.elapsed() > std::time::Duration::from_secs(2)
				{
					let kf_buf = bytes::Bytes::from_static(&[0x04]);
					let _ = dg_stats.send(kf_buf);
					last_keyframe_request = std::time::Instant::now();
				}
			}
		}
	});

	// Spawn task to forward broadcast datagrams to this connection's subscriber
	let dg_fwd = dg.clone();
	let forward_task = tokio::spawn(async move {
		loop {
			match datagram_rx.recv().await {
				Ok(data) => {
					if dg_fwd.send(data).is_err() {
						break;
					}
				}
				Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
					tracing::warn!("datagram forward lagged, dropped {} datagrams", n);
				}
				Err(_) => break,
			}
		}
	});

	// Handle incoming datagrams (cloq requests + video datagram relay)
	loop {
		let data = match dg.recv().await {
			Ok(data) => data,
			Err(_) => break,
		};

		match data.first() {
			Some(0x01) if data.len() == 9 => {
				// Cloq sync request
				let t2 = now_us();
				let mut resp = bytes::BytesMut::with_capacity(25);
				resp.extend_from_slice(&[0x02]);
				resp.extend_from_slice(&data[1..9]);
				resp.extend_from_slice(&t2.to_be_bytes());
				resp.extend_from_slice(&now_us().to_be_bytes());
				let _ = dg.send(resp.freeze());
			}
			Some(0x05) => {
				// Video datagram — broadcast to all other connections
				let _ = datagram_tx.send(data);
			}
			_ => {}
		}
	}

	stats_task.abort();
	forward_task.abort();
	tracing::debug!("datagram handler ended");
}

fn now_us() -> u64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.unwrap()
		.as_micros() as u64
}
