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

		// Spawn clock sync responder if datagram transport is available
		if let Some(dg) = dg_handle {
			tokio::spawn(run_clock_responder(dg));
		}

		tracing::info!(version = %session.version(), transport, "negotiated");

		// Wait until the session is closed.
		// Keep registration alive so the cluster node stays announced.
		session.closed().await?;
		drop(registration);
		Ok(())
	}
}

/// Responds to clock sync datagrams with relay timestamps.
///
/// Wire format:
///   Request  (9 bytes): [0x01][t1:u64 BE] — client local time in µs since epoch
///   Response (25 bytes): [0x02][t1:u64 echo][t2:u64 relay_rx][t3:u64 relay_tx]
///
/// Clients compute offset via NTP algorithm: offset = ((t2-t1) + (t3-t4)) / 2
async fn run_clock_responder(dg: moq_native::DatagramHandle) {
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

	tracing::debug!("clock sync responder ended");
}

fn now_us() -> u64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.unwrap()
		.as_micros() as u64
}
