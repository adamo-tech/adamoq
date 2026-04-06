//! Kendall's Tau trend detector with collapse window.
//!
//! Classifies a time series of samples as rising, falling, or indeterminate.
//! Uses Kendall's Tau rank correlation, which is robust to outliers and
//! doesn't assume a particular distribution.

use std::collections::VecDeque;
use std::time::Instant;

use super::config::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrendDirection {
	Upward,
	Downward,
	Stable,
	Inconclusive,
}

/// Tracks a time series of f64 samples and classifies the trend.
pub struct TrendDetector {
	samples: VecDeque<f64>,
	max_samples: usize,
	last_added: Option<(Instant, f64)>,
	upward_threshold: f64,
	downward_threshold: f64,
	min_samples: usize,
}

impl TrendDetector {
	pub fn new(max_samples: usize, min_samples: usize, downward_threshold: f64) -> Self {
		Self {
			samples: VecDeque::with_capacity(max_samples),
			max_samples,
			last_added: None,
			upward_threshold: TREND_UPWARD_THRESHOLD,
			downward_threshold,
			min_samples,
		}
	}

	/// Default detector for steady-state (non-probe) mode.
	pub fn steady_state() -> Self {
		Self::new(
			TREND_REQUIRED_SAMPLES,
			TREND_MIN_SAMPLES,
			TREND_DOWNWARD_THRESHOLD,
		)
	}

	/// Faster detector used during probing — reacts to small changes.
	pub fn probe_mode() -> Self {
		Self::new(
			TREND_PROBE_SAMPLES,
			TREND_PROBE_SAMPLES,
			TREND_DOWNWARD_PROBE,
		)
	}

	/// Add a sample. Duplicates arriving within TREND_COLLAPSE_WINDOW are skipped
	/// to prevent rapid-fire identical updates from dominating the trend.
	pub fn add(&mut self, value: f64, now: Instant) {
		if let Some((last_time, last_value)) = self.last_added {
			if now.duration_since(last_time) < TREND_COLLAPSE_WINDOW && (value - last_value).abs() < f64::EPSILON {
				return; // collapse duplicate
			}
		}

		self.samples.push_back(value);
		if self.samples.len() > self.max_samples {
			self.samples.pop_front();
		}
		self.last_added = Some((now, value));
	}

	/// Classify the current trend direction.
	pub fn direction(&self) -> TrendDirection {
		if self.samples.len() < self.min_samples {
			return TrendDirection::Inconclusive;
		}
		let tau = self.kendalls_tau();
		if tau > self.upward_threshold {
			TrendDirection::Upward
		} else if tau < self.downward_threshold {
			TrendDirection::Downward
		} else {
			TrendDirection::Stable
		}
	}

	/// Raw trend coefficient in [-1.0, +1.0].
	pub fn coefficient(&self) -> f64 {
		if self.samples.len() < 2 {
			return 0.0;
		}
		self.kendalls_tau()
	}

	/// Number of samples currently stored.
	pub fn len(&self) -> usize {
		self.samples.len()
	}

	pub fn is_empty(&self) -> bool {
		self.samples.is_empty()
	}

	/// Most recent sample, if any.
	pub fn latest(&self) -> Option<f64> {
		self.samples.back().copied()
	}

	/// Maximum sample in the window.
	pub fn max(&self) -> Option<f64> {
		self.samples.iter().copied().fold(None, |acc, x| match acc {
			None => Some(x),
			Some(a) => Some(a.max(x)),
		})
	}

	/// Minimum sample in the window.
	pub fn min(&self) -> Option<f64> {
		self.samples.iter().copied().fold(None, |acc, x| match acc {
			None => Some(x),
			Some(a) => Some(a.min(x)),
		})
	}

	fn kendalls_tau(&self) -> f64 {
		let n = self.samples.len();
		if n < 2 {
			return 0.0;
		}

		let mut concordant: i64 = 0;
		let mut discordant: i64 = 0;

		for i in 0..n - 1 {
			for j in i + 1..n {
				// j is later than i. Concordant = later sample is larger (rising).
				let a = self.samples[i];
				let b = self.samples[j];
				if b > a {
					concordant += 1;
				} else if b < a {
					discordant += 1;
				}
				// Ties contribute to neither.
			}
		}

		let total = concordant + discordant;
		if total == 0 {
			return 0.0;
		}
		(concordant - discordant) as f64 / total as f64
	}

	pub fn clear(&mut self) {
		self.samples.clear();
		self.last_added = None;
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::time::Duration;

	fn feed(d: &mut TrendDetector, values: &[f64]) {
		let mut t = Instant::now();
		for &v in values {
			d.add(v, t);
			t += Duration::from_millis(600); // past collapse window
		}
	}

	#[test]
	fn monotonic_rising_is_upward() {
		let mut d = TrendDetector::steady_state();
		feed(&mut d, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]);
		assert_eq!(d.direction(), TrendDirection::Upward);
		assert!(d.coefficient() > 0.9);
	}

	#[test]
	fn monotonic_falling_is_downward() {
		let mut d = TrendDetector::steady_state();
		feed(&mut d, &[8.0, 7.0, 6.0, 5.0, 4.0, 3.0, 2.0, 1.0]);
		assert_eq!(d.direction(), TrendDirection::Downward);
		assert!(d.coefficient() < -0.9);
	}

	#[test]
	fn noisy_stable_is_stable_or_inconclusive() {
		let mut d = TrendDetector::steady_state();
		feed(&mut d, &[5.0, 5.1, 4.9, 5.0, 5.1, 4.9, 5.0, 5.0]);
		let dir = d.direction();
		assert!(dir == TrendDirection::Stable || dir == TrendDirection::Inconclusive);
	}

	#[test]
	fn insufficient_samples_is_inconclusive() {
		let mut d = TrendDetector::steady_state();
		feed(&mut d, &[1.0, 2.0, 3.0]);
		assert_eq!(d.direction(), TrendDirection::Inconclusive);
	}

	#[test]
	fn window_evicts_old_samples() {
		let mut d = TrendDetector::new(4, 3, -0.5);
		feed(&mut d, &[10.0, 9.0, 8.0, 7.0, 6.0, 5.0]); // only last 4 kept
		assert_eq!(d.len(), 4);
		assert_eq!(d.direction(), TrendDirection::Downward);
	}

	#[test]
	fn collapses_duplicates_within_window() {
		let mut d = TrendDetector::steady_state();
		let t = Instant::now();
		d.add(5.0, t);
		d.add(5.0, t + Duration::from_millis(100)); // same value, within window
		d.add(5.0, t + Duration::from_millis(200));
		assert_eq!(d.len(), 1);
	}

	#[test]
	fn does_not_collapse_different_values() {
		let mut d = TrendDetector::steady_state();
		let t = Instant::now();
		d.add(5.0, t);
		d.add(6.0, t + Duration::from_millis(100)); // different value
		assert_eq!(d.len(), 2);
	}

	#[test]
	fn probe_mode_reacts_faster() {
		let mut d = TrendDetector::probe_mode();
		feed(&mut d, &[10.0, 9.0, 8.0]);
		assert_eq!(d.direction(), TrendDirection::Downward);
	}
}
