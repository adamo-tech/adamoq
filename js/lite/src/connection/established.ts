import type { Announced } from "../announced.ts";
import type { Broadcast } from "../broadcast.ts";
import type { SyncClock } from "../cloq.ts";
import type * as Path from "../path.ts";

// Both moq-lite and moq-ietf implement this.
export interface Established {
	readonly url: URL;
	readonly version: string;

	/** Relay-synced clock (NTP over QUIC datagrams) for RTT and clock offset. Null if unavailable. */
	readonly clock?: SyncClock | null;

	announced(prefix?: Path.Valid): Announced;
	publish(path: Path.Valid, broadcast: Broadcast): void;
	consume(broadcast: Path.Valid): Broadcast;
	close(): void;
	closed: Promise<void>;
	getStats?(): Promise<{ rttMs: number; smoothedRttMs: number } | undefined>;
}
