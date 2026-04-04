/**
 * cloq — relay-synced clock via WebTransport datagrams.
 *
 * Syncs the local clock to the relay using NTP-over-datagrams.
 * All participants sharing a relay will have a common time domain,
 * enabling accurate glass-to-glass latency and GCC one-way delay.
 *
 * Wire format:
 *   Request  (9 bytes):  [0x01][t1:u64 BE]  — client local time (µs since epoch)
 *   Response (25 bytes): [0x02][t1:u64 echo][t2:u64 relay_rx][t3:u64 relay_tx]
 */

import type { DatagramDispatcher } from "./datagram.ts";

const SYNC_INTERVAL_MS = 2000;
const EWMA_ALPHA = 0.1;
const RTT_HISTORY_SIZE = 8;

/** Returns monotonic microseconds since UNIX epoch. */
function localNowUs(): number {
	return (performance.timeOrigin + performance.now()) * 1000;
}

function writeU64BE(view: DataView, offset: number, value: number): void {
	const high = Math.floor(value / 0x100000000);
	const low = value >>> 0;
	view.setUint32(offset, high);
	view.setUint32(offset + 4, low);
}

function readU64BE(view: DataView, offset: number): number {
	const high = view.getUint32(offset);
	const low = view.getUint32(offset + 4);
	return high * 0x100000000 + low;
}

export class SyncClock {
	#offsetUs = 0;
	#rttUs = 0;
	#syncCount = 0;
	#running = true;
	#readyResolve!: () => void;
	#ready: Promise<void>;

	constructor(dispatcher: DatagramDispatcher) {
		this.#ready = new Promise((resolve) => {
			this.#readyResolve = resolve;
		});
		this.#run(dispatcher);
	}

	/** Relay-synced time in microseconds since UNIX epoch. */
	nowUs(): number {
		return localNowUs() + this.#offsetUs;
	}

	/** Current estimated offset (relay - local) in microseconds. */
	offsetUs(): number {
		return this.#offsetUs;
	}

	/** Last measured RTT in microseconds. */
	rttUs(): number {
		return this.#rttUs;
	}

	/** Number of successful sync exchanges. */
	syncCount(): number {
		return this.#syncCount;
	}

	/** Resolves after the first successful sync. */
	get ready(): Promise<void> {
		return this.#ready;
	}

	/** Stop the sync loop. */
	close(): void {
		this.#running = false;
	}

	async #run(dispatcher: DatagramDispatcher): Promise<void> {
		let smoothedOffset: number | null = null;
		const rttHistory: number[] = [];
		let pendingT1: number | null = null;

		// Register handler for cloq responses (0x02)
		dispatcher.on(0x02, (data: Uint8Array) => {
			if (data.byteLength !== 25) return;
			if (pendingT1 === null) return;

			const t4 = localNowUs();
			const view = new DataView(data.buffer, data.byteOffset, data.byteLength);
			const t1Echo = readU64BE(view, 1);

			if (t1Echo !== pendingT1) return; // stale
			const t1 = pendingT1;
			pendingT1 = null;

			const t2 = readU64BE(view, 9);
			const t3 = readU64BE(view, 17);

			// NTP calculation
			const rttSample = t4 - t1 - (t3 - t2);
			const offsetSample = ((t2 - t1) + (t3 - t4)) / 2;

			// RTT median filtering
			rttHistory.push(rttSample);
			if (rttHistory.length > RTT_HISTORY_SIZE) {
				rttHistory.shift();
			}

			if (rttHistory.length >= 3) {
				const sorted = [...rttHistory].sort((a, b) => a - b);
				const median = sorted[Math.floor(sorted.length / 2)];
				if (rttSample > median * 2) return;
			}

			// EWMA smoothing
			const alpha: number = smoothedOffset === null ? 1.0 : EWMA_ALPHA;
			smoothedOffset = alpha * offsetSample + (1 - alpha) * (smoothedOffset ?? offsetSample);

			this.#offsetUs = smoothedOffset;
			this.#rttUs = rttSample;
			this.#syncCount++;

			if (this.#syncCount === 1) {
				this.#readyResolve();
			}

			if (this.#syncCount <= 3 || this.#syncCount % 10 === 0) {
				console.log(
					`[cloq] offset=${Math.round(smoothedOffset)}us rtt=${Math.round(rttSample)}us (sync #${this.#syncCount})`,
				);
			}
		});

		// Send sync requests periodically
		while (this.#running) {
			const t1 = localNowUs();
			pendingT1 = t1;

			const req = new ArrayBuffer(9);
			const reqView = new DataView(req);
			reqView.setUint8(0, 0x01);
			writeU64BE(reqView, 1, t1);

			try {
				await dispatcher.send(new Uint8Array(req));
			} catch {
				break;
			}

			await sleep(SYNC_INTERVAL_MS);
		}
	}
}

function sleep(ms: number): Promise<void> {
	return new Promise((resolve) => setTimeout(resolve, ms));
}
