//! bitrate-sim — verification tool for the BitrateController.
//!
//! Replays a scripted network scenario through the controller and prints
//! each state transition + ceiling decision as it happens.
//!
//! Usage:
//!   cargo run --release --bin bitrate-sim -- --scenario clean_then_congested
//!   cargo run --release --bin bitrate-sim -- --scenario loss_spike
//!   cargo run --release --bin bitrate-sim -- --csv > output.csv
//!
//! Scenarios simulate QUIC transport stats at 500ms ticks:
//!   - estimated_send_rate_bps: what BBR thinks the pipe can handle
//!   - packets_sent/lost: for loss rate calculation
//!   - rtt_us: round-trip time
//!
//! Output shows for each tick:
//!   time | estimate | loss | rtt | trend | state | ceiling | probe?

use std::time::{Duration, Instant};

use clap::Parser;
use adamoq_bitrate::{AllocatorState, BitrateController, TransportStats};

#[derive(Parser)]
#[command(name = "bitrate-sim")]
struct Cli {
	/// Scenario name. Use "list" to see available scenarios.
	#[arg(long, default_value = "clean_then_congested")]
	scenario: String,

	/// Publisher target bitrate in kbps.
	#[arg(long, default_value_t = 2000)]
	publisher_kbps: u64,

	/// Output CSV instead of pretty-printed log.
	#[arg(long, default_value_t = false)]
	csv: bool,
}

/// One tick of network conditions at a specific simulation time.
#[derive(Debug, Clone)]
struct Tick {
	elapsed_ms: u64,
	estimated_send_rate_bps: u64,
	interval_sent: u64,
	interval_lost: u64,
	rtt_us: u64,
}

fn main() {
	let cli = Cli::parse();

	let scenario = match build_scenario(&cli.scenario, cli.publisher_kbps * 1000) {
		Some(s) => s,
		None => {
			list_scenarios();
			std::process::exit(1);
		}
	};

	if cli.csv {
		println!("elapsed_ms,estimate_bps,loss_rate,rtt_us,trend,state,ceiling_bps,probe_target_bps");
	} else {
		println!(
			"{:>6} │ {:>9} │ {:>5} │ {:>5} │ {:>12} │ {:>13} │ {:>9} │ probe",
			"t(ms)", "est(kbps)", "loss", "rtt", "trend", "state", "ceil(kbps)"
		);
		println!("{}", "─".repeat(100));
	}

	let start = Instant::now();
	let mut ctrl = BitrateController::new(cli.publisher_kbps * 1000, start);

	let mut cum_sent = 0u64;
	let mut cum_lost = 0u64;
	let mut prev_state = AllocatorState::Stable;
	let mut signals_emitted = 0u64;

	for tick in &scenario {
		cum_sent += tick.interval_sent;
		cum_lost += tick.interval_lost;
		let now = start + Duration::from_millis(tick.elapsed_ms);

		let stats = TransportStats {
			estimated_send_rate_bps: tick.estimated_send_rate_bps,
			packets_sent: cum_sent,
			packets_lost: cum_lost,
			bytes_sent: 0,
			rtt_us: tick.rtt_us,
		};

		let signal = ctrl.update(stats, now);

		if cli.csv {
			let trend = format!("{:?}", signal.as_ref().map(|s| s.estimate.trend).unwrap_or(adamoq_bitrate::TrendDirection::Inconclusive));
			let state = format!("{:?}", ctrl.state());
			let ceiling = ctrl.ceiling_bps();
			let probe_target = signal.as_ref().and_then(|s| s.probe.as_ref()).map(|p| p.target_bps).unwrap_or(0);
			let loss = signal.as_ref().map(|s| s.estimate.loss_rate).unwrap_or(0.0);
			let est = signal.as_ref().map(|s| s.estimate.bps).unwrap_or(0);
			println!(
				"{},{},{:.4},{},{},{},{},{}",
				tick.elapsed_ms, est, loss, tick.rtt_us, trend, state, ceiling, probe_target
			);
		} else if let Some(sig) = &signal {
			signals_emitted += 1;
			let transition = if sig.state != prev_state { "★" } else { " " };
			prev_state = sig.state;
			let probe_str = sig.probe
				.as_ref()
				.map(|p| format!(" probe→{}kbps", p.target_bps / 1000))
				.unwrap_or_default();
			println!(
				"{}{:>5} │ {:>9} │ {:>4.1}% │ {:>3}ms │ {:>12} │ {:>13} │ {:>9} │{}",
				transition,
				tick.elapsed_ms,
				sig.estimate.bps / 1000,
				sig.estimate.loss_rate * 100.0,
				sig.estimate.rtt_us / 1000,
				format!("{:?}", sig.estimate.trend),
				format!("{:?}", sig.state),
				sig.ceiling_bps / 1000,
				probe_str,
			);
		}
	}

	if !cli.csv {
		println!("{}", "─".repeat(100));
		println!(
			"scenario={} ticks={} signals_emitted={} final_state={:?} final_ceiling={}kbps",
			cli.scenario,
			scenario.len(),
			signals_emitted,
			ctrl.state(),
			ctrl.ceiling_bps() / 1000,
		);
	}
}

fn list_scenarios() {
	eprintln!("Available scenarios:");
	eprintln!("  clean                — 2Mbps link, no loss, steady");
	eprintln!("  clean_then_congested — 2Mbps → gradually drops to 800kbps, then recovers");
	eprintln!("  loss_spike           — clean baseline, 20% loss for 10s, then clears");
	eprintln!("  flapping             — alternating good/bad every 5s");
	eprintln!("  slow_drain           — linear 2Mbps → 400kbps over 60s");
	eprintln!("  cliff                — 2Mbps, then instant drop to 500kbps");
}

fn build_scenario(name: &str, target_bps: u64) -> Option<Vec<Tick>> {
	// Base: 100 packets sent per 500ms tick at 2Mbps baseline.
	let base_sent = 100;
	match name {
		"clean" => Some((0..60).map(|i| Tick {
			elapsed_ms: i * 500,
			estimated_send_rate_bps: target_bps * 120 / 100, // 20% headroom
			interval_sent: base_sent,
			interval_lost: 0,
			rtt_us: 50_000,
		}).collect()),

		"clean_then_congested" => {
			let mut ticks = Vec::new();
			// 0-10s: clean
			for i in 0..20 {
				ticks.push(Tick {
					elapsed_ms: i * 500,
					estimated_send_rate_bps: target_bps * 120 / 100,
					interval_sent: base_sent,
					interval_lost: 0,
					rtt_us: 50_000,
				});
			}
			// 10-30s: estimate drops from 100% → 40% of target
			for i in 20..60 {
				let progress = (i - 20) as i64; // 0..40
				let pct = (100 - progress * 60 / 40).max(40) as u64; // 100 → 40
				ticks.push(Tick {
					elapsed_ms: i * 500,
					estimated_send_rate_bps: target_bps * pct / 100,
					interval_sent: base_sent,
					interval_lost: if pct < 80 { 3 } else { 0 },
					rtt_us: 50_000 + (100 - pct) * 1000, // RTT rises with congestion
				});
			}
			// 30-60s: recovery
			for i in 60..120 {
				ticks.push(Tick {
					elapsed_ms: i * 500,
					estimated_send_rate_bps: target_bps * 120 / 100,
					interval_sent: base_sent,
					interval_lost: 0,
					rtt_us: 50_000,
				});
			}
			Some(ticks)
		}

		"loss_spike" => {
			let mut ticks = Vec::new();
			for i in 0..80 {
				let loss_active = i >= 20 && i < 40;
				ticks.push(Tick {
					elapsed_ms: i * 500,
					estimated_send_rate_bps: target_bps * 120 / 100,
					interval_sent: base_sent,
					interval_lost: if loss_active { 20 } else { 0 },
					rtt_us: 50_000,
				});
			}
			Some(ticks)
		}

		"flapping" => {
			let mut ticks = Vec::new();
			for i in 0..120 {
				let bad = (i / 10) % 2 == 1; // bad every other 5s
				ticks.push(Tick {
					elapsed_ms: i * 500,
					estimated_send_rate_bps: if bad { target_bps * 50 / 100 } else { target_bps * 120 / 100 },
					interval_sent: base_sent,
					interval_lost: if bad { 10 } else { 0 },
					rtt_us: if bad { 150_000 } else { 50_000 },
				});
			}
			Some(ticks)
		}

		"slow_drain" => {
			let mut ticks = Vec::new();
			for i in 0..120 {
				let pct = ((100 - (i * 80 / 120) as i64).max(20)) as u64; // 100 → 20
				ticks.push(Tick {
					elapsed_ms: i * 500,
					estimated_send_rate_bps: target_bps * pct / 100,
					interval_sent: base_sent,
					interval_lost: 0,
					rtt_us: 50_000 + (100 - pct) * 500,
				});
			}
			Some(ticks)
		}

		"cliff" => {
			let mut ticks = Vec::new();
			for i in 0..60 {
				let pct = if i < 20 { 120 } else { 25 };
				ticks.push(Tick {
					elapsed_ms: i * 500,
					estimated_send_rate_bps: target_bps * pct / 100,
					interval_sent: base_sent,
					interval_lost: if i >= 20 && i < 30 { 15 } else { 0 },
					rtt_us: 50_000,
				});
			}
			Some(ticks)
		}

		_ => None,
	}
}
