import type { Signal } from "@moq/signals";
import type { Announced } from "../announced.ts";
import type { Bandwidth } from "../bandwidth.ts";
import type { Broadcast } from "../broadcast.ts";
import type { SyncClock } from "../cloq.ts";
import type { DatagramDispatcher } from "../datagram.ts";
import type * as Path from "../path.ts";
import type * as Time from "../time.ts";

// Both moq-lite and moq-ietf implement this.
export interface Established {
	readonly url: URL;
	readonly version: string;

	/** Datagram dispatcher for multiplexing QUIC datagrams by type byte. */
	readonly datagrams?: DatagramDispatcher | null;

	/** Relay-synced clock (NTP over QUIC datagrams) for RTT and clock offset. Null if unavailable. */
	readonly clock?: SyncClock | null;

	/** Estimated send bitrate from the congestion controller (if supported). */
	readonly sendBandwidth?: Bandwidth;

	/** Estimated receive bitrate from PROBE (moq-lite-03+ only). */
	readonly recvBandwidth?: Bandwidth;

	/** RTT in milliseconds from PROBE (moq-lite-04+ only). */
	readonly rtt?: Signal<Time.Milli | undefined>;

	announced(prefix?: Path.Valid): Announced;
	publish(path: Path.Valid, broadcast: Broadcast): void;
	consume(broadcast: Path.Valid): Broadcast;
	close(): void;
	closed: Promise<void>;
	getStats?(): Promise<{ rttMs: number; smoothedRttMs: number } | undefined>;
}
