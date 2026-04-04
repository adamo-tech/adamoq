/**
 * cloq — relay clock sync via heartbeat datagrams.
 *
 * The relay broadcasts its clock every 500ms via 0x0A heartbeat datagrams.
 * Clients compute offset as (relay_time - local_time).
 *
 * Wire format:
 *   Heartbeat (9 bytes): [0x0A][relay_time_us:u64 BE]
 */

import type { DatagramDispatcher } from "./datagram.ts";

/** Returns monotonic microseconds since UNIX epoch. */
function localNowUs(): number {
	return (performance.timeOrigin + performance.now()) * 1000;
}

function readU64BE(view: DataView, offset: number): number {
	const high = view.getUint32(offset);
	const low = view.getUint32(offset + 4);
	return high * 0x100000000 + low;
}

export class SyncClock {
	#offsetUs = 0;
	#syncCount = 0;
	#readyResolve!: () => void;
	#ready: Promise<void>;

	constructor(dispatcher: DatagramDispatcher) {
		this.#ready = new Promise((resolve) => {
			this.#readyResolve = resolve;
		});

		// Listen for relay clock heartbeat (0x0A)
		dispatcher.on(0x0a, (data: Uint8Array) => {
			if (data.byteLength !== 9) return;

			const view = new DataView(data.buffer, data.byteOffset, data.byteLength);
			const relayTime = readU64BE(view, 1);
			const localTime = localNowUs();

			this.#offsetUs = relayTime - localTime;
			this.#syncCount++;

			if (this.#syncCount === 1) {
				this.#readyResolve();
			}

			if (this.#syncCount <= 3 || this.#syncCount % 30 === 0) {
				console.log(`[cloq] offset=${Math.round(this.#offsetUs)}us (heartbeat #${this.#syncCount})`);
			}
		});
	}

	/** Relay-synced time in microseconds since UNIX epoch. */
	nowUs(): number {
		return localNowUs() + this.#offsetUs;
	}

	/** Current estimated offset (relay - local) in microseconds. */
	offsetUs(): number {
		return this.#offsetUs;
	}

	/** RTT not measured via heartbeat — use WebTransport stats instead. */
	rttUs(): number {
		return 0;
	}

	/** Number of heartbeats received. */
	syncCount(): number {
		return this.#syncCount;
	}

	/** Resolves after the first heartbeat. */
	get ready(): Promise<void> {
		return this.#ready;
	}

	close(): void {
		// No background task to stop — handler is passive.
	}
}
