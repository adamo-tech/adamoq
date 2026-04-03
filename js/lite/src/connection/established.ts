import type { Announced } from "../announced.ts";
import type { Broadcast } from "../broadcast.ts";
import type * as Path from "../path.ts";

// Both moq-lite and moq-ietf implement this.
export interface Established {
	readonly url: URL;
	readonly version: string;

	announced(prefix?: Path.Valid): Announced;
	publish(path: Path.Valid, broadcast: Broadcast): void;
	consume(broadcast: Path.Valid): Broadcast;
	close(): void;
	closed: Promise<void>;

	/** Estimated receive bitrate in bits/second from probe messages, or undefined if unavailable. */
	readonly estimatedRecvRate?: number | undefined;

	/** Callback invoked when the estimated receive rate changes. */
	onRecvRate?: ((rate: number | undefined) => void) | undefined;
}
