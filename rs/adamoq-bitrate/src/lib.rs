//! Variable-bitrate control for a MoQ subscriber connection.
//!
//! Ported from LiveKit's stream allocator (pkg/sfu/streamallocator/), adapted
//! for a single-stream, single-ceiling per-subscriber architecture.
//!
//! Usage:
//! ```ignore
//! let mut ctrl = BitrateController::new(target_bps, now);
//! loop {
//!     let stats = TransportStats { ... };  // from QUIC
//!     if let Some(signal) = ctrl.update(stats, now) {
//!         // signal.ceiling_bps — send as 0x07 to publisher
//!         // signal.probe — send probe traffic if Some
//!     }
//! }
//! ```

mod allocator;
mod config;
mod estimator;
mod prober;
mod trend;

use std::time::{Duration, Instant};

pub use allocator::AllocatorState;
pub use estimator::{Estimate, TransportStats};
pub use prober::ProbeRequest;
pub use trend::TrendDirection;

use allocator::Allocator;
use config::{MIN_SIGNAL_INTERVAL, WARMUP_DURATION};
use estimator::BandwidthEstimator;

/// Emitted when the controller wants to signal a change to the publisher.
#[derive(Debug, Clone)]
pub struct BitrateSignal {
	pub ceiling_bps: u64,
	pub state: AllocatorState,
	pub probe: Option<ProbeRequest>,
	/// Latest estimate at time of signal (for logging).
	pub estimate: Estimate,
}

/// Per-subscriber bitrate controller. Fed QUIC transport stats every tick
/// (typically every 500ms), emits signals when the state changes or
/// periodically when non-stable.
pub struct BitrateController {
	estimator: BandwidthEstimator,
	allocator: Allocator,
	created: Instant,
	last_signal: Instant,
	last_emitted_state: AllocatorState,
	last_emitted_ceiling: u64,
}

impl BitrateController {
	pub fn new(publisher_target_bps: u64, now: Instant) -> Self {
		Self {
			estimator: BandwidthEstimator::new(),
			allocator: Allocator::new(publisher_target_bps, now),
			created: now,
			last_signal: now,
			last_emitted_state: AllocatorState::Stable,
			last_emitted_ceiling: publisher_target_bps,
		}
	}

	pub fn set_publisher_target(&mut self, bps: u64) {
		self.allocator.set_publisher_target(bps);
	}

	pub fn state(&self) -> AllocatorState {
		self.allocator.state()
	}

	pub fn ceiling_bps(&self) -> u64 {
		self.allocator.ceiling_bps()
	}

	/// Feed QUIC stats. Returns a signal when the controller wants the
	/// publisher to change ceiling, or when a probe should be dispatched.
	pub fn update(&mut self, stats: TransportStats, now: Instant) -> Option<BitrateSignal> {
		let estimate = self.estimator.update(stats, now);

		// Don't make decisions during warmup.
		if now.duration_since(self.created) < WARMUP_DURATION {
			return None;
		}

		let decision = self.allocator.update(&estimate, now);

		let state_changed = decision.state != self.last_emitted_state;
		let ceiling_changed = decision.ceiling_bps != self.last_emitted_ceiling;
		let should_refresh = decision.state != AllocatorState::Stable
			&& now.duration_since(self.last_signal) >= MIN_SIGNAL_INTERVAL;
		let has_probe = decision.probe.is_some();

		if state_changed || ceiling_changed || should_refresh || has_probe {
			if state_changed {
				tracing::info!(
					prev = ?self.last_emitted_state,
					new = ?decision.state,
					ceiling_kbps = decision.ceiling_bps / 1000,
					estimate_kbps = estimate.bps / 1000,
					loss = format!("{:.1}%", estimate.loss_rate * 100.0),
					rtt_ms = estimate.rtt_us / 1000,
					"bitrate state transition"
				);
			}
			self.last_signal = now;
			self.last_emitted_state = decision.state;
			self.last_emitted_ceiling = decision.ceiling_bps;

			return Some(BitrateSignal {
				ceiling_bps: decision.ceiling_bps,
				state: decision.state,
				probe: decision.probe,
				estimate,
			});
		}

		None
	}

	/// Time since the controller was created.
	pub fn age(&self, now: Instant) -> Duration {
		now.duration_since(self.created)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn make_stats(bps: u64, sent: u64, lost: u64) -> TransportStats {
		TransportStats {
			estimated_send_rate_bps: bps,
			packets_sent: sent,
			packets_lost: lost,
			bytes_sent: 0,
			rtt_us: 50_000,
		}
	}

	#[test]
	fn warmup_suppresses_signals() {
		let now = Instant::now();
		let mut c = BitrateController::new(2_000_000, now);
		// Feed a dramatic drop during warmup.
		let signal = c.update(make_stats(100_000, 100, 50), now);
		assert!(signal.is_none(), "should not emit during warmup");
	}

	#[test]
	fn emits_signal_on_state_change() {
		let now = Instant::now();
		let mut c = BitrateController::new(2_000_000, now);
		let past_warmup = now + WARMUP_DURATION + Duration::from_secs(1);

		// Establish a declining trend (enough samples).
		let mut sent = 0u64;
		let mut t = past_warmup;
		for i in 0..15 {
			sent += 100;
			let bps = 2_000_000 - (i as u64 * 100_000);
			c.update(make_stats(bps, sent, 0), t);
			t += Duration::from_millis(600);
		}
		// At this point the allocator should have transitioned.
		// (The signal is captured on the update that transitions.)
		assert_ne!(c.state(), AllocatorState::Stable);
	}
}
