//! Bandwidth estimator: consumes raw QUIC transport stats, produces a
//! smoothed bandwidth estimate with trend classification.
//!
//! Inputs (per tick, typically every 500ms):
//!   - estimated_send_rate (bps) — BBR's estimate from QUIC
//!   - delta_packets_sent, delta_packets_lost — for loss rate
//!   - rtt_us — round-trip time
//!
//! Outputs:
//!   - smoothed_estimate_bps
//!   - trend direction
//!   - loss_rate (EWMA)

use std::time::Instant;

use super::config::*;
use super::trend::{TrendDetector, TrendDirection};

/// Input stats sampled from QUIC transport.
#[derive(Debug, Clone, Copy)]
pub struct TransportStats {
	pub estimated_send_rate_bps: u64,
	pub packets_sent: u64,
	pub packets_lost: u64,
	pub bytes_sent: u64,
	pub rtt_us: u64,
}

/// Current output of the estimator.
#[derive(Debug, Clone, Copy)]
pub struct Estimate {
	/// Smoothed bandwidth estimate in bps.
	pub bps: u64,
	/// Raw (un-smoothed) BBR estimate in bps — reacts immediately to send rate changes.
	pub raw_bps: u64,
	/// EWMA-smoothed packet loss rate in [0, 1].
	pub loss_rate: f64,
	/// RTT in microseconds.
	pub rtt_us: u64,
	/// Trend direction of the bandwidth estimate over the recent window.
	pub trend: TrendDirection,
	/// Raw trend coefficient in [-1, +1].
	pub trend_coefficient: f64,
}

pub struct BandwidthEstimator {
	baseline_bps: f64,
	loss_rate: f64,
	prev_packets_sent: u64,
	prev_packets_lost: u64,
	trend: TrendDetector,
	initialized: bool,
}

impl BandwidthEstimator {
	pub fn new() -> Self {
		Self {
			baseline_bps: 0.0,
			loss_rate: 0.0,
			prev_packets_sent: 0,
			prev_packets_lost: 0,
			trend: TrendDetector::steady_state(),
			initialized: false,
		}
	}

	pub fn update(&mut self, stats: TransportStats, now: Instant) -> Estimate {
		// Skip if no data yet.
		if stats.estimated_send_rate_bps == 0 {
			return Estimate {
				bps: self.baseline_bps as u64,
				raw_bps: 0,
				loss_rate: self.loss_rate,
				rtt_us: stats.rtt_us,
				trend: TrendDirection::Inconclusive,
				trend_coefficient: 0.0,
			};
		}

		let raw = stats.estimated_send_rate_bps as f64;

		// EWMA smoothing of the bandwidth estimate.
		if !self.initialized {
			self.baseline_bps = raw;
			self.initialized = true;
		} else {
			self.baseline_bps = ESTIMATE_BASELINE_ALPHA * raw + (1.0 - ESTIMATE_BASELINE_ALPHA) * self.baseline_bps;
		}

		// Feed smoothed estimate into trend detector.
		self.trend.add(self.baseline_bps, now);

		// Loss rate with asymmetric EWMA (fast attack, slow decay).
		let delta_sent = stats.packets_sent.saturating_sub(self.prev_packets_sent);
		let delta_lost = stats.packets_lost.saturating_sub(self.prev_packets_lost);
		self.prev_packets_sent = stats.packets_sent;
		self.prev_packets_lost = stats.packets_lost;

		let interval_loss = if delta_sent + delta_lost > 0 {
			delta_lost as f64 / (delta_sent + delta_lost) as f64
		} else {
			0.0
		};
		let loss_alpha = if interval_loss > self.loss_rate { 0.5 } else { 0.1 };
		self.loss_rate = loss_alpha * interval_loss + (1.0 - loss_alpha) * self.loss_rate;

		Estimate {
			bps: self.baseline_bps as u64,
			raw_bps: stats.estimated_send_rate_bps,
			loss_rate: self.loss_rate,
			rtt_us: stats.rtt_us,
			trend: self.trend.direction(),
			trend_coefficient: self.trend.coefficient(),
		}
	}

	/// Reset trend state — useful when entering/leaving probing.
	pub fn reset_trend(&mut self) {
		self.trend.clear();
	}
}

impl Default for BandwidthEstimator {
	fn default() -> Self {
		Self::new()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::time::Duration;

	fn make_stats(bps: u64, sent: u64, lost: u64, rtt_us: u64) -> TransportStats {
		TransportStats {
			estimated_send_rate_bps: bps,
			packets_sent: sent,
			packets_lost: lost,
			bytes_sent: 0,
			rtt_us,
		}
	}

	#[test]
	fn steady_estimate_converges() {
		let mut e = BandwidthEstimator::new();
		let mut t = Instant::now();
		let mut sent = 0u64;
		for _ in 0..20 {
			sent += 100;
			let est = e.update(make_stats(1_000_000, sent, 0, 50_000), t);
			assert!(est.bps > 0);
			t += Duration::from_millis(500);
		}
		let est = e.update(make_stats(1_000_000, sent + 100, 0, 50_000), t);
		// Should converge to ~1Mbps.
		assert!(est.bps > 900_000 && est.bps < 1_100_000);
		assert!(est.loss_rate < 0.01);
	}

	#[test]
	fn declining_bandwidth_detected_as_downward() {
		let mut e = BandwidthEstimator::new();
		let mut t = Instant::now();
		let mut sent = 0u64;
		// Start at 2Mbps, drop to 500kbps over 15 samples.
		for i in 0..15 {
			sent += 100;
			let bps = 2_000_000 - (i as u64 * 100_000);
			e.update(make_stats(bps, sent, 0, 50_000), t);
			t += Duration::from_millis(600);
		}
		let est = e.update(make_stats(500_000, sent + 100, 0, 50_000), t);
		assert_eq!(est.trend, TrendDirection::Downward);
	}

	#[test]
	fn loss_rate_tracks_packet_loss() {
		let mut e = BandwidthEstimator::new();
		let mut t = Instant::now();
		// 10% loss rate: 100 sent, 10 lost per tick.
		let mut sent = 0u64;
		let mut lost = 0u64;
		for _ in 0..10 {
			sent += 100;
			lost += 10;
			e.update(make_stats(1_000_000, sent, lost, 50_000), t);
			t += Duration::from_millis(500);
		}
		let est = e.update(make_stats(1_000_000, sent + 100, lost + 10, 50_000), t);
		// sent+lost = 110, lost=10 → 10/110 ≈ 0.09
		assert!(est.loss_rate > 0.05 && est.loss_rate < 0.15);
	}
}
