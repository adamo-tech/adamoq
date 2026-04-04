/**
 * Datagram dispatcher — multiplexes QUIC datagrams by type byte.
 *
 * Reads all datagrams from the WebTransport session and routes them
 * to registered handlers based on the first byte:
 *   0x01/0x02 — cloq (clock sync)
 *   0x03     — relay stats
 *   0x04     — relay keyframe request
 *   0x05     — video datagram (fragmented frame)
 *   0x10     — generic topic message (future)
 */

export type DatagramHandler = (data: Uint8Array) => void;

export class DatagramDispatcher {
	#handlers = new Map<number, DatagramHandler>();
	#writer: WritableStreamDefaultWriter<Uint8Array>;
	#running = true;

	constructor(transport: WebTransport) {
		this.#writer = transport.datagrams.writable.getWriter();
		this.#run(transport);
	}

	/** Register a handler for a datagram type byte. */
	on(type: number, handler: DatagramHandler): void {
		this.#handlers.set(type, handler);
	}

	/** Send a datagram. */
	async send(data: Uint8Array): Promise<void> {
		await this.#writer.write(data);
	}

	close(): void {
		this.#running = false;
		this.#writer.releaseLock();
	}

	async #run(transport: WebTransport): Promise<void> {
		const reader = transport.datagrams.readable.getReader();

		try {
			while (this.#running) {
				const result = await reader.read();
				if (result.done) break;

				const data = result.value as Uint8Array;
				if (data.length === 0) continue;

				const type = data[0];
				const handler = this.#handlers.get(type);
				if (handler) {
					handler(data);
				}
			}
		} catch {
			// Transport closed
		} finally {
			reader.releaseLock();
		}
	}
}
