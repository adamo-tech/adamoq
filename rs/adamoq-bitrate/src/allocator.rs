//! Allocator: the state machine that translates bandwidth estimates and
//! congestion signals into a per-subscriber bitrate ceiling.
//!
//! States:
//!   STABLE        — ceiling = publisher target, no constraint
//!   EARLY_WARNING — congestion suspected, hold current ceiling (don't increase)
//!   DEFICIENT     — ceiling reduced below target, waiting for probe recovery
//!   PROBING       — actively testing if headroom has returned
//!
//! Ceiling computation is proportional to the bandwidth estimate, smoothed by
//! the trend detector and loss-based override. No hardcoded reduction tiers.

use std::time::Instant;

use super::config::*;
use super::estimator::Estimate;
use super::prober::{ProbeRequest, ProbeResult, Prober};
use super::trend::TrendDirection;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocatorState {
	Stable,
	EarlyWarning,
	Deficient,
	Probing,
}

impl AllocatorState {
	pub fn as_u8(self) -> u8 {
		match self {
			AllocatorState::Stable => 0,
			AllocatorState::EarlyWarning => 1,
			AllocatorState::Deficient => 2,
			AllocatorState::Probing => 3,
		}
	}
}

/// Decision emitted by the allocator.
#[derive(Debug, Clone)]
pub struct AllocatorDecision {
	pub state: AllocatorState,
	pub ceiling_bps: u64,
	pub probe: Option<ProbeRequest>,
}

pub struct Allocator {
	state: AllocatorState,
	publisher_target_bps: u64,
	committed_ceiling_bps: u64,
	/// When Probing, this is the probe target we're testing.
	active_probe_target_bps: u64,
	prober: Prober,
	last_state_change: Instant,
}

impl Allocator {
	pub fn new(publisher_target_bps: u64, now: Instant) -> Self {
		Self {
			state: AllocatorState::Stable,
			publisher_target_bps,
			committed_ceiling_bps: publisher_target_bps,
			active_probe_target_bps: 0,
			prober: Prober::new(now),
			last_state_change: now,
		}
	}

	pub fn state(&self) -> AllocatorState {
		self.state
	}

	pub fn ceiling_bps(&self) -> u64 {
		self.committed_ceiling_bps
	}

	pub fn set_publisher_target(&mut self, bps: u64) {
		self.publisher_target_bps = bps;
		// If we're stable, the ceiling tracks the target.
		if self.state == AllocatorState::Stable {
			self.committed_ceiling_bps = bps;
		}
	}

	/// Advance the allocator with new bandwidth estimate. Returns a decision
	/// that should be acted on (may include a probe request).
	pub fn update(&mut self, estimate: &Estimate, now: Instant) -> AllocatorDecision {
		// Feed RAW estimate to prober — the smoothed value can't catch up in time
		// during a 500ms probe window.
		self.prober.observe_estimate(estimate.raw_bps);

		// Detect congestion from the estimate + loss.
		let congested = self.detect_congestion(estimate);
		if congested && self.state == AllocatorState::Probing {
			self.prober.mark_congested();
		}

		// Tick the prober — may finalize a probe.
		if let Some(result) = self.prober.tick(now, estimate.rtt_us) {
			self.handle_probe_result(result, now);
		}

		// Evaluate state transitions based on current estimate + trend.
		self.evaluate_transitions(estimate, congested, now);

		// Compute ceiling based on state.
		self.update_ceiling(estimate);

		// Maybe start a new probe.
		let probe_req = if self.state == AllocatorState::Deficient {
			if let Some(req) = self.prober.maybe_start(self.committed_ceiling_bps, now, estimate.rtt_us) {
				self.active_probe_target_bps = req.target_bps;
				self.transition(AllocatorState::Probing, now);
				Some(req)
			} else {
				None
			}
		} else {
			None
		};

		// During Probing, emit the probe target as the ceiling — publisher encodes
		// at that rate so BBR can measure whether the pipe handles it.
		let emitted_ceiling = if self.state == AllocatorState::Probing && self.active_probe_target_bps > 0 {
			self.active_probe_target_bps
		} else {
			self.committed_ceiling_bps
		};

		AllocatorDecision {
			state: self.state,
			ceiling_bps: emitted_ceiling,
			probe: probe_req,
		}
	}

	fn detect_congestion(&self, estimate: &Estimate) -> bool {
		// Primary signal: bandwidth estimate trending downward.
		if estimate.trend == TrendDirection::Downward {
			return true;
		}
		// Secondary: loss rate elevated.
		if estimate.loss_rate > LOSS_TRIGGER_RATIO * 2.0 {
			return true;
		}
		false
	}

	fn evaluate_transitions(&mut self, estimate: &Estimate, congested: bool, now: Instant) {
		match self.state {
			AllocatorState::Stable => {
				// Drop to deficient on clear congestion, or early warning on stable-but-elevated loss.
				if congested && estimate.bps < self.publisher_target_bps {
					self.transition(AllocatorState::Deficient, now);
				} else if estimate.loss_rate > LOSS_TRIGGER_RATIO {
					self.transition(AllocatorState::EarlyWarning, now);
				}
			}
			AllocatorState::EarlyWarning => {
				if congested {
					self.transition(AllocatorState::Deficient, now);
				} else if estimate.loss_rate < LOSS_TRIGGER_RATIO / 2.0
					&& estimate.trend != TrendDirection::Downward
				{
					self.transition(AllocatorState::Stable, now);
				}
			}
			AllocatorState::Deficient => {
				// Return to stable if estimate recovers above publisher target.
				if estimate.bps >= self.publisher_target_bps
					&& estimate.loss_rate < LOSS_TRIGGER_RATIO
					&& estimate.trend != TrendDirection::Downward
				{
					self.transition(AllocatorState::Stable, now);
					self.prober.cancel(now);
				}
			}
			AllocatorState::Probing => {
				// Probing transitions are handled by probe result.
				// But drop to deficient if congestion clearly detected.
				if congested {
					self.prober.mark_congested();
				}
			}
		}
	}

	fn handle_probe_result(&mut self, result: ProbeResult, now: Instant) {
		self.active_probe_target_bps = 0;
		match result {
			ProbeResult::Success { peak_estimate_bps } => {
				// Raise ceiling to the observed peak (capped by publisher target).
				self.committed_ceiling_bps = peak_estimate_bps.min(self.publisher_target_bps);
				if self.committed_ceiling_bps >= self.publisher_target_bps {
					self.transition(AllocatorState::Stable, now);
				} else {
					self.transition(AllocatorState::Deficient, now);
				}
			}
			ProbeResult::Failed | ProbeResult::Congested => {
				self.transition(AllocatorState::Deficient, now);
			}
		}
	}

	fn update_ceiling(&mut self, estimate: &Estimate) {
		let min_ceiling = (self.publisher_target_bps as f64 * MIN_CEILING_RATIO) as u64;

		match self.state {
			AllocatorState::Stable => {
				self.committed_ceiling_bps = self.publisher_target_bps;
			}
			AllocatorState::EarlyWarning => {
				// Hold at current ceiling. Don't reduce unless we drop to deficient.
			}
			AllocatorState::Deficient => {
				// Only ratchet ceiling DOWN on active congestion — otherwise hold steady
				// and let probing move us up. Prevents BBR noise from walking us down.
				let congesting = estimate.trend == TrendDirection::Downward
					|| estimate.loss_rate > LOSS_TRIGGER_RATIO;
				if !congesting {
					return;
				}

				let mut target = estimate.bps;

				// Loss-based reduction: reduce beyond pure estimate if loss is high.
				if estimate.loss_rate > LOSS_TRIGGER_RATIO {
					let loss_ceiling =
						(self.publisher_target_bps as f64 * (1.0 - LOSS_ATTENUATOR * estimate.loss_rate).max(0.2)) as u64;
					target = target.min(loss_ceiling);
				}

				// Hysteresis: only commit a reduction if new target is clearly below current.
				let commit_threshold = (self.committed_ceiling_bps as f64 * COMMIT_THRESHOLD_RATIO) as u64;
				if target < commit_threshold {
					self.committed_ceiling_bps = target.max(min_ceiling);
				}
			}
			AllocatorState::Probing => {
				// Don't change ceiling during probe — probe finalization decides.
			}
		}
	}

	fn transition(&mut self, new_state: AllocatorState, now: Instant) {
		if new_state != self.state {
			self.state = new_state;
			self.last_state_change = now;
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use super::super::trend::TrendDirection;

	fn make_estimate(bps: u64, loss: f64, trend: TrendDirection) -> Estimate {
		Estimate {
			bps,
			raw_bps: bps,
			loss_rate: loss,
			rtt_us: 50_000,
			trend,
			trend_coefficient: 0.0,
		}
	}

	#[test]
	fn starts_stable_at_target() {
		let now = Instant::now();
		let a = Allocator::new(2_000_000, now);
		assert_eq!(a.state(), AllocatorState::Stable);
		assert_eq!(a.ceiling_bps(), 2_000_000);
	}

	#[test]
	fn stays_stable_under_good_conditions() {
		let now = Instant::now();
		let mut a = Allocator::new(2_000_000, now);
		let est = make_estimate(2_500_000, 0.0, TrendDirection::Stable);
		let d = a.update(&est, now);
		assert_eq!(d.state, AllocatorState::Stable);
		assert_eq!(d.ceiling_bps, 2_000_000);
	}

	#[test]
	fn drops_to_deficient_on_downward_trend() {
		let now = Instant::now();
		let mut a = Allocator::new(2_000_000, now);
		let est = make_estimate(800_000, 0.01, TrendDirection::Downward);
		let d = a.update(&est, now);
		assert_eq!(d.state, AllocatorState::Deficient);
		assert!(d.ceiling_bps < 2_000_000);
	}

	#[test]
	fn early_warning_on_moderate_loss() {
		let now = Instant::now();
		let mut a = Allocator::new(2_000_000, now);
		// Elevated loss but stable trend.
		let est = make_estimate(2_000_000, 0.08, TrendDirection::Stable);
		let d = a.update(&est, now);
		assert_eq!(d.state, AllocatorState::EarlyWarning);
	}

	#[test]
	fn returns_to_stable_on_recovery() {
		let now = Instant::now();
		let mut a = Allocator::new(2_000_000, now);
		// Drop to deficient.
		a.update(&make_estimate(500_000, 0.02, TrendDirection::Downward), now);
		assert_eq!(a.state(), AllocatorState::Deficient);
		// Estimate recovers fully.
		let est = make_estimate(2_500_000, 0.0, TrendDirection::Stable);
		let d = a.update(&est, now);
		assert_eq!(d.state, AllocatorState::Stable);
		assert_eq!(d.ceiling_bps, 2_000_000);
	}

	#[test]
	fn publisher_target_update_propagates() {
		let now = Instant::now();
		let mut a = Allocator::new(2_000_000, now);
		a.set_publisher_target(3_000_000);
		assert_eq!(a.ceiling_bps(), 3_000_000);
	}

	#[test]
	fn ceiling_does_not_drift_down_without_congestion_signal() {
		// Regression: in Deficient state with flat estimate and no loss, the
		// ceiling must not ratchet down due to BBR estimate noise.
		let now = Instant::now();
		let mut a = Allocator::new(8_000_000, now);
		// Drop to Deficient.
		a.update(&make_estimate(1_500_000, 0.02, TrendDirection::Downward), now);
		assert_eq!(a.state(), AllocatorState::Deficient);
		let initial_ceiling = a.ceiling_bps();

		// Now feed a flat estimate below initial — no trend, no loss.
		// Simulates steady-state where publisher encodes at ceiling and BBR's
		// estimate hovers with some noise.
		for i in 0..20 {
			let jitter = if i % 2 == 0 { -50_000 } else { 50_000 };
			let noisy = (initial_ceiling as i64 + jitter).max(0) as u64;
			a.update(&make_estimate(noisy, 0.0, TrendDirection::Stable), now);
		}
		// Ceiling should not have dropped — no congestion signal.
		assert_eq!(a.ceiling_bps(), initial_ceiling, "ceiling drifted without congestion signal");
	}

	#[test]
	fn ceiling_respects_minimum() {
		let now = Instant::now();
		let mut a = Allocator::new(2_000_000, now);
		// Drive estimate to near zero with high loss.
		let est = make_estimate(10_000, 0.50, TrendDirection::Downward);
		let d = a.update(&est, now);
		let min = (2_000_000.0 * MIN_CEILING_RATIO) as u64;
		assert!(d.ceiling_bps >= min);
	}

	#[test]
	fn ceiling_ratchets_down_only_on_active_congestion() {
		let now = Instant::now();
		let mut a = Allocator::new(8_000_000, now);
		// Enter Deficient.
		a.update(&make_estimate(2_000_000, 0.02, TrendDirection::Downward), now);
		let start_ceiling = a.ceiling_bps();

		// Feed a DECLINING estimate with downward trend — SHOULD ratchet down.
		a.update(&make_estimate(1_500_000, 0.0, TrendDirection::Downward), now);
		assert!(a.ceiling_bps() < start_ceiling, "should ratchet down on active congestion");
	}

	#[test]
	fn ceiling_ratchets_down_on_loss_even_if_trend_flat() {
		let now = Instant::now();
		let mut a = Allocator::new(8_000_000, now);
		// Enter Deficient first.
		a.update(&make_estimate(2_000_000, 0.10, TrendDirection::Downward), now);
		let start_ceiling = a.ceiling_bps();

		// Sustained loss, trend flat — should still ratchet.
		a.update(&make_estimate(1_500_000, 0.10, TrendDirection::Stable), now);
		assert!(a.ceiling_bps() <= start_ceiling, "should ratchet down on sustained loss");
	}

	#[test]
	fn early_warning_holds_ceiling_does_not_reduce() {
		let now = Instant::now();
		let mut a = Allocator::new(4_000_000, now);
		// Enter EarlyWarning via moderate loss.
		a.update(&make_estimate(4_000_000, 0.08, TrendDirection::Stable), now);
		assert_eq!(a.state(), AllocatorState::EarlyWarning);
		let ew_ceiling = a.ceiling_bps();

		// Another EarlyWarning tick — ceiling should NOT decrease.
		a.update(&make_estimate(3_500_000, 0.08, TrendDirection::Stable), now);
		assert_eq!(a.ceiling_bps(), ew_ceiling, "EarlyWarning holds ceiling");
	}

	#[test]
	fn early_warning_clears_to_stable() {
		let now = Instant::now();
		let mut a = Allocator::new(4_000_000, now);
		a.update(&make_estimate(4_000_000, 0.08, TrendDirection::Stable), now);
		assert_eq!(a.state(), AllocatorState::EarlyWarning);
		// Loss clears below half the trigger threshold.
		a.update(&make_estimate(4_000_000, 0.01, TrendDirection::Stable), now);
		assert_eq!(a.state(), AllocatorState::Stable);
	}

	#[test]
	fn deficient_does_not_reduce_below_commit_threshold() {
		// Small drops should not commit a new ceiling (hysteresis).
		let now = Instant::now();
		let mut a = Allocator::new(4_000_000, now);
		// Enter Deficient.
		a.update(&make_estimate(2_000_000, 0.10, TrendDirection::Downward), now);
		let ceiling_before = a.ceiling_bps();

		// Estimate drops but only by 3% (below 5% commit threshold).
		let tiny_drop = (ceiling_before as f64 * 0.97) as u64;
		a.update(&make_estimate(tiny_drop, 0.10, TrendDirection::Downward), now);
		assert_eq!(a.ceiling_bps(), ceiling_before, "tiny drop below commit threshold should not reduce");

		// Bigger drop (20%) SHOULD commit.
		let big_drop = (ceiling_before as f64 * 0.80) as u64;
		a.update(&make_estimate(big_drop, 0.10, TrendDirection::Downward), now);
		assert!(a.ceiling_bps() < ceiling_before, "drop past commit threshold should reduce");
	}

	#[test]
	fn loss_based_reduction_math() {
		// At 20% loss: ceiling = target * (1 - 1.5*0.20) = target * 0.70
		let now = Instant::now();
		let mut a = Allocator::new(1_000_000, now);
		a.update(&make_estimate(900_000, 0.20, TrendDirection::Downward), now);
		// Expected loss ceiling: 1_000_000 * 0.70 = 700_000
		// Committed is min(estimate=900k, loss_ceiling=700k) = 700k
		assert!(a.ceiling_bps() <= 700_000 + 50_000); // allow rounding
		assert!(a.ceiling_bps() >= 650_000);
	}

	#[test]
	fn loss_based_reduction_floor() {
		// At 80% loss, formula would go negative — must clamp to 20% of target.
		let now = Instant::now();
		let mut a = Allocator::new(1_000_000, now);
		a.update(&make_estimate(500_000, 0.80, TrendDirection::Downward), now);
		let min = (1_000_000.0 * MIN_CEILING_RATIO) as u64;
		assert!(a.ceiling_bps() >= min, "ceiling must respect MIN_CEILING_RATIO");
	}

	#[test]
	fn publisher_target_change_in_stable_updates_ceiling() {
		let now = Instant::now();
		let mut a = Allocator::new(2_000_000, now);
		let d = a.update(&make_estimate(2_500_000, 0.0, TrendDirection::Stable), now);
		assert_eq!(d.ceiling_bps, 2_000_000);

		a.set_publisher_target(5_000_000);
		let d = a.update(&make_estimate(6_000_000, 0.0, TrendDirection::Stable), now);
		assert_eq!(d.ceiling_bps, 5_000_000, "ceiling follows new target in Stable");
	}

	#[test]
	fn publisher_target_change_in_deficient_preserves_ceiling() {
		// When in Deficient, changing publisher_target shouldn't suddenly bump ceiling.
		let now = Instant::now();
		let mut a = Allocator::new(8_000_000, now);
		a.update(&make_estimate(2_000_000, 0.10, TrendDirection::Downward), now);
		let ceiling_before = a.ceiling_bps();
		a.set_publisher_target(12_000_000);
		assert_eq!(a.ceiling_bps(), ceiling_before, "target bump in Deficient preserves committed ceiling");
	}

	#[test]
	fn rapid_fluctuation_does_not_drain_ceiling() {
		// Alternating up/down around the ceiling shouldn't ratchet ceiling toward zero.
		let now = Instant::now();
		let mut a = Allocator::new(8_000_000, now);
		a.update(&make_estimate(2_000_000, 0.02, TrendDirection::Downward), now);
		let initial = a.ceiling_bps();

		// Alternate noisy up/down with Stable trend.
		for i in 0..30 {
			let bps = if i % 2 == 0 { initial + 100_000 } else { initial - 100_000 };
			a.update(&make_estimate(bps, 0.0, TrendDirection::Stable), now);
		}
		assert_eq!(a.ceiling_bps(), initial, "flapping without congestion should not drift ceiling");
	}

	#[test]
	fn estimate_at_publisher_target_returns_stable_from_deficient() {
		let now = Instant::now();
		let mut a = Allocator::new(4_000_000, now);
		// Force Deficient.
		a.update(&make_estimate(1_000_000, 0.05, TrendDirection::Downward), now);
		assert_eq!(a.state(), AllocatorState::Deficient);

		// Estimate fully recovers to target.
		a.update(&make_estimate(4_000_000, 0.0, TrendDirection::Stable), now);
		assert_eq!(a.state(), AllocatorState::Stable);
		assert_eq!(a.ceiling_bps(), 4_000_000);
	}
}
