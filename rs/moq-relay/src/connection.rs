use crate::{Auth, AuthParams, Cluster};

use axum::http;
use moq_native::Request;

pub struct Connection {
	pub id: u64,
	pub request: Request,
	pub cluster: Cluster,
	pub auth: Auth,
}

impl Connection {
	#[tracing::instrument("conn", skip_all, fields(id = self.id))]
	pub async fn run(self) -> anyhow::Result<()> {
		let params = match self.request.url() {
			Some(url) => AuthParams::from_url(url),
			None => AuthParams::default(),
		};

		// Verify the URL before accepting the connection.
		let token = match self.auth.verify(&params) {
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

		// Spawn datagram handler (cloq clock sync + transport stats feedback)
		if let Some(dg) = dg_handle {
			tokio::spawn(run_datagram_handler(dg));
		}

		tracing::info!(version = %session.version(), transport, "negotiated");

		// Wait until the session is closed.
		// Keep registration alive so the cluster node stays announced.
		session.closed().await?;
		drop(registration);
		Ok(())
	}
}

/// Handles datagrams: clock sync (cloq) + transport stats feedback.
///
/// Cloq wire format:
///   Request  (9 bytes): [0x01][t1:u64 BE] — client local time in µs since epoch
///   Response (25 bytes): [0x02][t1:u64 echo][t2:u64 relay_rx][t3:u64 relay_tx]
///
/// Transport stats feedback (relay → publisher, every 500ms):
///   [0x03][recv_bytes:u64 BE][recv_packets:u64 BE][lost_packets:u64 BE]
///   [rtt_us:u64 BE][cwnd:u64 BE][timestamp_us:u64 BE]
///
/// The publisher uses these to compute goodput, loss rate, and delay variation
/// on the publisher→relay path (the robot's upload link).
async fn run_datagram_handler(dg: moq_native::DatagramHandle) {
	use web_transport_trait::Stats;

	// Spawn stats feedback sender
	let dg_stats = dg.clone();
	let stats_task = tokio::spawn(async move {
		let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
		interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

		loop {
			interval.tick().await;

			let stats = dg_stats.stats();
			let rtt_us = stats.rtt()
				.map(|r| r.as_micros() as u64)
				.unwrap_or(0);

			let mut buf = bytes::BytesMut::with_capacity(49);
			buf.extend_from_slice(&[0x03]);
			buf.extend_from_slice(&stats.bytes_received().unwrap_or(0).to_be_bytes());
			buf.extend_from_slice(&stats.packets_received().unwrap_or(0).to_be_bytes());
			buf.extend_from_slice(&stats.packets_lost().unwrap_or(0).to_be_bytes());
			buf.extend_from_slice(&rtt_us.to_be_bytes());
			buf.extend_from_slice(&(stats.estimated_send_rate().unwrap_or(0)).to_be_bytes());
			buf.extend_from_slice(&now_us().to_be_bytes());

			if dg_stats.send(buf.freeze()).is_err() {
				break;
			}
		}
	});

	// Handle incoming datagrams (cloq requests)
	loop {
		let data = match dg.recv().await {
			Ok(data) => data,
			Err(_) => break,
		};

		if data.len() == 9 && data[0] == 0x01 {
			let t2 = now_us();

			let mut resp = bytes::BytesMut::with_capacity(25);
			resp.extend_from_slice(&[0x02]);
			resp.extend_from_slice(&data[1..9]); // echo t1
			resp.extend_from_slice(&t2.to_be_bytes());
			resp.extend_from_slice(&now_us().to_be_bytes()); // t3

			let _ = dg.send(resp.freeze());
		}
	}

	stats_task.abort();
	tracing::debug!("datagram handler ended");
}

fn now_us() -> u64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.unwrap()
		.as_micros() as u64
}
