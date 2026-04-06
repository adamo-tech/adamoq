//! Probe controller: decides WHEN to probe, the probe bitrate goal, and
//! interprets the result.
//!
//! When the allocator is in DEFICIENT state, the prober periodically requests
//! a higher bitrate than the current committed ceiling to test if headroom
//! has returned. If the estimate rises to meet or exceed the probe goal
//! without triggering congestion, the ceiling is raised.
//!
//! Backoff: failed probes double the wait interval (up to PROBE_MAX_INTERVAL).
//! Successful probes reset to PROBE_BASE_INTERVAL.

use std::time::{Duration, Instant};

use super::config::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeState {
	/// Not probing. Waiting for next probe attempt.
	Idle,
	/// Currently sending probe traffic.
	Active,
	/// Probe ended, waiting for settle period before evaluating.
	Settling,
}

/// Result of evaluating a completed probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeResult {
	/// Probe succeeded — bandwidth estimate reached or exceeded the goal.
	/// New ceiling should be set to the observed peak.
	Success { peak_estimate_bps: u64 },
	/// Probe failed — estimate didn't rise to meet the goal.
	/// Back off and retry later.
	Failed,
	/// Congestion detected DURING probe — abort, stay conservative.
	Congested,
}

/// A probe request emitted to the transport layer (relay sends probe traffic).
#[derive(Debug, Clone, Copy)]
pub struct ProbeRequest {
	/// Target bitrate for the probe (includes current usage + probe headroom).
	pub target_bps: u64,
	/// How long to send probe traffic.
	pub duration: Duration,
}

pub struct Prober {
	state: ProbeState,
	next_attempt: Instant,
	current_interval: Duration,
	probe_start: Option<Instant>,
	probe_goal_bps: u64,
	peak_estimate_during_probe: u64,
	congested_during_probe: bool,
}

impl Prober {
	pub fn new(now: Instant) -> Self {
		Self {
			state: ProbeState::Idle,
			next_attempt: now + PROBE_BASE_INTERVAL,
			current_interval: PROBE_BASE_INTERVAL,
			probe_start: None,
			probe_goal_bps: 0,
			peak_estimate_during_probe: 0,
			congested_during_probe: false,
		}
	}

	pub fn state(&self) -> ProbeState {
		self.state
	}

	pub fn is_active(&self) -> bool {
		matches!(self.state, ProbeState::Active | ProbeState::Settling)
	}

	/// Check if a new probe should be started. Returns the probe request if yes.
	pub fn maybe_start(&mut self, current_ceiling_bps: u64, now: Instant, rtt_us: u64) -> Option<ProbeRequest> {
		if self.state != ProbeState::Idle {
			return None;
		}
		if now < self.next_attempt {
			return None;
		}

		// Probe target = current ceiling * overage ratio, with a minimum absolute headroom.
		let probe_headroom = ((current_ceiling_bps as f64 * (PROBE_OVERAGE_RATIO - 1.0)) as u64).max(PROBE_MIN_BPS);
		let target_bps = current_ceiling_bps + probe_headroom;

		self.state = ProbeState::Active;
		self.probe_start = Some(now);
		self.probe_goal_bps = target_bps;
		self.peak_estimate_during_probe = 0;
		self.congested_during_probe = false;

		// Settle wait is PROBE_SETTLE_RTTS * RTT after probe duration ends.
		let _settle_wait = Duration::from_micros(rtt_us * PROBE_SETTLE_RTTS as u64);

		Some(ProbeRequest {
			target_bps,
			duration: PROBE_DURATION,
		})
	}

	/// Feed bandwidth estimate samples during an active probe.
	pub fn observe_estimate(&mut self, estimate_bps: u64) {
		if self.state == ProbeState::Active && estimate_bps > self.peak_estimate_during_probe {
			self.peak_estimate_during_probe = estimate_bps;
		}
	}

	/// Signal that congestion was detected during the probe.
	pub fn mark_congested(&mut self) {
		if self.state == ProbeState::Active {
			self.congested_during_probe = true;
		}
	}

	/// Advance the state machine. Call on each tick. Returns a ProbeResult
	/// when a probe completes.
	pub fn tick(&mut self, now: Instant, rtt_us: u64) -> Option<ProbeResult> {
		// Active → Settling (drop through on same tick if time has elapsed)
		if self.state == ProbeState::Active {
			if let Some(start) = self.probe_start {
				if now.duration_since(start) >= PROBE_DURATION {
					self.state = ProbeState::Settling;
				}
			}
		}
		// Settling → Idle (with result)
		if self.state == ProbeState::Settling {
			let settle_wait = Duration::from_micros((rtt_us * PROBE_SETTLE_RTTS as u64).max(250_000));
			if let Some(start) = self.probe_start {
				let total_wait = PROBE_DURATION + settle_wait;
				if now.duration_since(start) >= total_wait {
					return Some(self.finalize(now));
				}
			}
		}
		None
	}

	fn finalize(&mut self, now: Instant) -> ProbeResult {
		self.state = ProbeState::Idle;
		self.probe_start = None;

		let result = if self.congested_during_probe {
			self.back_off(now);
			ProbeResult::Congested
		} else if self.peak_estimate_during_probe >= self.probe_goal_bps {
			self.reset_interval(now);
			ProbeResult::Success {
				peak_estimate_bps: self.peak_estimate_during_probe,
			}
		} else {
			self.back_off(now);
			ProbeResult::Failed
		};

		self.peak_estimate_during_probe = 0;
		self.congested_during_probe = false;
		result
	}

	fn back_off(&mut self, now: Instant) {
		self.current_interval = Duration::from_secs_f64(
			(self.current_interval.as_secs_f64() * PROBE_BACKOFF_FACTOR).min(PROBE_MAX_INTERVAL.as_secs_f64()),
		);
		self.next_attempt = now + self.current_interval;
	}

	fn reset_interval(&mut self, now: Instant) {
		self.current_interval = PROBE_BASE_INTERVAL;
		self.next_attempt = now + self.current_interval;
	}

	/// Called when we leave DEFICIENT state — cancel pending probe.
	pub fn cancel(&mut self, now: Instant) {
		self.state = ProbeState::Idle;
		self.probe_start = None;
		self.peak_estimate_during_probe = 0;
		self.congested_during_probe = false;
		self.reset_interval(now);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn idle_initially() {
		let now = Instant::now();
		let p = Prober::new(now);
		assert_eq!(p.state(), ProbeState::Idle);
	}

	#[test]
	fn starts_probe_after_interval() {
		let now = Instant::now();
		let mut p = Prober::new(now);
		assert!(p.maybe_start(1_000_000, now, 50_000).is_none());
		let after = now + PROBE_BASE_INTERVAL + Duration::from_millis(100);
		let req = p.maybe_start(1_000_000, after, 50_000);
		assert!(req.is_some());
		assert_eq!(p.state(), ProbeState::Active);
		// Target should be ceiling + overage (1M * 0.2 = 200k, max with MIN=200k).
		assert_eq!(req.unwrap().target_bps, 1_200_000);
	}

	#[test]
	fn successful_probe_resets_interval() {
		let now = Instant::now();
		let mut p = Prober::new(now);
		let after = now + PROBE_BASE_INTERVAL + Duration::from_millis(100);
		let req = p.maybe_start(1_000_000, after, 50_000).unwrap();
		p.observe_estimate(1_300_000); // exceeds goal
		// Advance past probe duration + settle window.
		let settle_wait = Duration::from_micros(50_000 * PROBE_SETTLE_RTTS as u64).max(Duration::from_millis(250));
		let done = after + req.duration + settle_wait + Duration::from_millis(10);
		let result = p.tick(done, 50_000);
		match result {
			Some(ProbeResult::Success { peak_estimate_bps }) => {
				assert_eq!(peak_estimate_bps, 1_300_000);
			}
			other => panic!("expected Success, got {:?}", other),
		}
		assert_eq!(p.state(), ProbeState::Idle);
	}

	#[test]
	fn failed_probe_backs_off() {
		let now = Instant::now();
		let mut p = Prober::new(now);
		let after = now + PROBE_BASE_INTERVAL + Duration::from_millis(100);
		let req = p.maybe_start(1_000_000, after, 50_000).unwrap();
		// No estimate observed during probe (peak stays 0, below goal).
		let settle_wait = Duration::from_micros(50_000 * PROBE_SETTLE_RTTS as u64).max(Duration::from_millis(250));
		let done = after + req.duration + settle_wait + Duration::from_millis(10);
		let result = p.tick(done, 50_000);
		assert_eq!(result, Some(ProbeResult::Failed));

		// Next attempt should be further out (backoff).
		let interval_after = p.current_interval;
		assert!(interval_after > PROBE_BASE_INTERVAL);
	}

	#[test]
	fn congested_during_probe_backs_off() {
		let now = Instant::now();
		let mut p = Prober::new(now);
		let after = now + PROBE_BASE_INTERVAL + Duration::from_millis(100);
		let req = p.maybe_start(1_000_000, after, 50_000).unwrap();
		p.observe_estimate(2_000_000); // would be success
		p.mark_congested(); // but congestion wins
		let settle_wait = Duration::from_micros(50_000 * PROBE_SETTLE_RTTS as u64).max(Duration::from_millis(250));
		let done = after + req.duration + settle_wait + Duration::from_millis(10);
		let result = p.tick(done, 50_000);
		assert_eq!(result, Some(ProbeResult::Congested));
	}

	#[test]
	fn cancel_returns_to_idle() {
		let now = Instant::now();
		let mut p = Prober::new(now);
		let after = now + PROBE_BASE_INTERVAL + Duration::from_millis(100);
		p.maybe_start(1_000_000, after, 50_000);
		assert!(p.is_active());
		p.cancel(after);
		assert_eq!(p.state(), ProbeState::Idle);
	}
}
