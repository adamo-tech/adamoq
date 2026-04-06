//! Tunable constants for the bitrate controller.
//!
//! Values mirror LiveKit's defaults where possible, adjusted for QUIC
//! transport stats (coarser than per-packet TWCC feedback).

use std::time::Duration;

// ── Trend detection ─────────────────────────────────────────────────────────

/// Number of samples required to detect a trend (non-probe mode).
/// More samples = more stable, slower to react.
pub const TREND_REQUIRED_SAMPLES: usize = 12;

/// Minimum samples for accepting a downward trend.
pub const TREND_MIN_SAMPLES: usize = 8;

/// Samples required during probing (faster detection).
pub const TREND_PROBE_SAMPLES: usize = 3;

/// Samples are collapsed if they arrive within this window — prevents
/// duplicate/rapid-fire updates from dominating the trend.
pub const TREND_COLLAPSE_WINDOW: Duration = Duration::from_millis(500);

/// Downward trend threshold (Kendall's Tau). Must be more negative than this
/// to classify as a downward trend. Range: [-1.0, +1.0].
pub const TREND_DOWNWARD_THRESHOLD: f64 = -0.6;

/// Probe-mode downward threshold — stricter (any downward motion).
pub const TREND_DOWNWARD_PROBE: f64 = 0.0;

/// Upward trend threshold — when we're willing to probe higher.
pub const TREND_UPWARD_THRESHOLD: f64 = 0.4;

// ── Bandwidth estimation ────────────────────────────────────────────────────

/// EWMA alpha for the bandwidth estimate baseline (low-pass filter).
/// Lower = slower, smoother estimate.
pub const ESTIMATE_BASELINE_ALPHA: f64 = 0.1;

/// Only commit a lower ceiling if estimate drops below this fraction of
/// current expected usage — hysteresis to prevent noise-driven oscillation.
pub const COMMIT_THRESHOLD_RATIO: f64 = 0.95;

// ── Loss-based reduction ────────────────────────────────────────────────────

/// NACK/loss ratio above which loss-based reduction kicks in.
pub const LOSS_TRIGGER_RATIO: f64 = 0.05;

/// How strongly loss reduces ceiling. ceiling = expected * (1 - ATTENUATOR * loss).
pub const LOSS_ATTENUATOR: f64 = 1.5;

// ── Probing ─────────────────────────────────────────────────────────────────

/// Minimum wait between probe attempts.
pub const PROBE_BASE_INTERVAL: Duration = Duration::from_secs(3);

/// Maximum wait between probes when backing off repeatedly.
pub const PROBE_MAX_INTERVAL: Duration = Duration::from_secs(120);

/// Exponential backoff factor after a failed probe.
pub const PROBE_BACKOFF_FACTOR: f64 = 1.5;

/// Duration to send probe traffic before evaluating.
pub const PROBE_DURATION: Duration = Duration::from_millis(500);

/// Probe target ratio above current expected usage (120% = +20%).
pub const PROBE_OVERAGE_RATIO: f64 = 1.20;

/// Minimum absolute probe headroom in bits per second.
pub const PROBE_MIN_BPS: u64 = 200_000;

/// How many RTTs to wait after probe ends before evaluating result.
pub const PROBE_SETTLE_RTTS: u32 = 5;

// ── State machine ──────────────────────────────────────────────────────────

/// Warmup period at startup — collect baseline before making decisions.
pub const WARMUP_DURATION: Duration = Duration::from_secs(5);

/// Minimum interval between emitting 0x07 ceiling signals.
pub const MIN_SIGNAL_INTERVAL: Duration = Duration::from_secs(1);

/// Minimum ceiling as a fraction of publisher target — don't starve the stream.
pub const MIN_CEILING_RATIO: f64 = 0.20;
