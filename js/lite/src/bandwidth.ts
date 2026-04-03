import { type Getter, Signal } from "@moq/signals";

/**
 * A bandwidth estimate that can be read synchronously or observed reactively.
 *
 * Created internally by the connection. Consumers read from it via the signal.
 */
export class Bandwidth {
	#bitrate = new Signal<number | undefined>(undefined);

	/** Reactive signal for the current bandwidth estimate in bits per second. */
	readonly signal: Getter<number | undefined> = this.#bitrate;

	/**
	 * Update the bandwidth estimate. Called internally by the connection/subscriber.
	 * @internal
	 */
	set(bitrate: number | undefined): void {
		this.#bitrate.set(bitrate);
	}

	/** Get the current bandwidth estimate synchronously. */
	get(): number | undefined {
		return this.#bitrate.peek();
	}

	/** Wait for the bandwidth estimate to change. */
	async changed(): Promise<number | undefined> {
		return new Promise<number | undefined>((resolve) => {
			this.#bitrate.changed(resolve);
		});
	}
}
