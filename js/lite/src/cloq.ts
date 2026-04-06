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
	#rttUs = 0;
	#syncCount = 0;
	#readyResolve!: () => void;
	#ready: Promise<void>;
	#pendingPingT1: number | null = null;

	constructor(dispatcher: DatagramDispatcher, _transport?: WebTransport) {
		this.#ready = new Promise((resolve) => {
			this.#readyResolve = resolve;
		});

		// Listen for relay clock heartbeat (0x0A)
		dispatcher.on(0x0a, (data: Uint8Array) => {
			if (data.byteLength !== 9) return;

			const view = new DataView(data.buffer, data.byteOffset, data.byteLength);
			const relayTime = readU64BE(view, 1);
			const localTime = localNowUs();

			// Correct with half-RTT (from ping responses) for accurate offset
			this.#offsetUs = relayTime - localTime + this.#rttUs / 2;
			this.#syncCount++;

			if (this.#syncCount === 1) {
				this.#readyResolve();
			}

			if (this.#syncCount <= 3 || this.#syncCount % 30 === 0) {
				console.log(
					`[cloq] offset=${Math.round(this.#offsetUs)}us rtt=${Math.round(this.#rttUs)}us (heartbeat #${this.#syncCount})`,
				);
			}
		});

		// Listen for 0x02 ping responses for RTT measurement
		dispatcher.on(0x02, (data: Uint8Array) => {
			if (data.byteLength !== 25) return;
			if (this.#pendingPingT1 === null) return;
			const t4 = localNowUs();
			const view = new DataView(data.buffer, data.byteOffset, data.byteLength);
			const t1Echo = readU64BE(view, 1);
			if (t1Echo !== this.#pendingPingT1) return;
			const t2 = readU64BE(view, 9);
			const t3 = readU64BE(view, 17);
			this.#pendingPingT1 = null;
			const rttSample = t4 - t1Echo - (t3 - t2);
			if (rttSample > 0 && rttSample < 10_000_000) {
				this.#rttUs = this.#rttUs === 0 ? rttSample : 0.3 * rttSample + 0.7 * this.#rttUs;
			}
		});

		// Send periodic RTT pings
		this.#runPings(dispatcher);
	}

	async #runPings(dispatcher: DatagramDispatcher): Promise<void> {
		await this.#ready;
		while (true) {
			const t1 = localNowUs();
			this.#pendingPingT1 = t1;
			const req = new ArrayBuffer(9);
			const view = new DataView(req);
			view.setUint8(0, 0x01);
			const high = Math.floor(t1 / 0x100000000);
			const low = t1 >>> 0;
			view.setUint32(1, high);
			view.setUint32(5, low);
			try {
				await dispatcher.send(new Uint8Array(req));
			} catch {
				break;
			}
			await new Promise((r) => setTimeout(r, 2000));
		}
	}

	/** Relay-synced time in microseconds since UNIX epoch. */
	nowUs(): number {
		return localNowUs() + this.#offsetUs;
	}

	/** Current estimated offset (relay - local) in microseconds. */
	offsetUs(): number {
		return this.#offsetUs;
	}

	/** Last known RTT from ping/pong in microseconds. */
	rttUs(): number {
		return this.#rttUs;
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
