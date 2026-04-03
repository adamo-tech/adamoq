use std::{
	future::Future,
	pin::Pin,
	sync::{
		Arc,
		atomic::{self, AtomicU64},
	},
};

use web_transport_trait::Stats;

use crate::{Error, Version};

/// A MoQ transport session, wrapping a WebTransport connection.
///
/// Created via:
/// - [`crate::Client::connect`] for clients.
/// - [`crate::Server::accept`] for servers.
#[derive(Clone)]
pub struct Session {
	session: Arc<dyn SessionInner>,
	version: Version,
	closed: bool,
	estimated_recv_rate: Arc<AtomicU64>,
}

impl Session {
	pub(super) fn new<S: web_transport_trait::Session>(session: S, version: Version) -> Self {
		Self {
			session: Arc::new(session),
			version,
			closed: false,
			estimated_recv_rate: Arc::new(AtomicU64::new(0)),
		}
	}

	pub(super) fn with_recv_rate(mut self, rate: Arc<AtomicU64>) -> Self {
		self.estimated_recv_rate = rate;
		self
	}

	/// Returns the negotiated protocol version.
	pub fn version(&self) -> Version {
		self.version
	}

	/// Returns the estimated send rate in bits/second from the QUIC congestion controller.
	/// Returns `None` if the transport doesn't support this metric.
	pub fn estimated_send_rate(&self) -> Option<u64> {
		self.session.estimated_send_rate()
	}

	/// Returns the estimated receive rate in bits/second from probe messages.
	/// Returns `None` if no probe data has been received yet.
	pub fn estimated_recv_rate(&self) -> Option<u64> {
		match self.estimated_recv_rate.load(atomic::Ordering::Relaxed) {
			0 => None,
			rate => Some(rate),
		}
	}

	/// Close the underlying transport session.
	pub fn close(&mut self, err: Error) {
		if self.closed {
			return;
		}
		self.closed = true;
		self.session.close(err.to_code(), err.to_string().as_ref());
	}

	/// Block until the transport session is closed.
	// TODO Remove the Result the next time we make a breaking change.
	pub async fn closed(&self) -> Result<(), Error> {
		self.session.closed().await;
		Err(Error::Transport)
	}
}

impl Drop for Session {
	fn drop(&mut self) {
		if !self.closed {
			self.session.close(Error::Cancel.to_code(), "dropped");
		}
	}
}

// We use a wrapper type that is dyn-compatible to remove the generic bounds from Session.
trait SessionInner: Send + Sync {
	fn close(&self, code: u32, reason: &str);
	fn closed(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
	fn estimated_send_rate(&self) -> Option<u64>;
}

impl<S: web_transport_trait::Session> SessionInner for S {
	fn close(&self, code: u32, reason: &str) {
		S::close(self, code, reason);
	}

	fn closed(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
		Box::pin(async move {
			let _ = S::closed(self).await;
		})
	}

	fn estimated_send_rate(&self) -> Option<u64> {
		self.stats().estimated_send_rate()
	}
}
