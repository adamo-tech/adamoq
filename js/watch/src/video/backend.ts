import type * as Moq from "@moq/lite";
import type { Getter } from "@moq/signals";
import type { BufferedRanges } from "../backend";
import type { Source } from "./source";

// Video specific signals that work regardless of the backend source (mse vs webcodecs).
export interface Backend {
	// The source of the video.
	source: Source;

	// The stats of the video.
	stats: Getter<Stats | undefined>;

	// Whether the video is currently buffering
	stalled: Getter<boolean>;

	// Buffered time ranges (for MSE backend).
	buffered: Getter<BufferedRanges>;

	// The timestamp of the current frame.
	timestamp: Getter<Moq.Time.Milli | undefined>;
}

export interface Stats {
	frameCount: number;
	bytesReceived: number;
	/** Avg time from frame received → decoder.decode() (ms) */
	depacketizeMs: number;
	/** Avg time from decoder.decode() → output callback (ms) */
	decodeMs: number;
	/** Avg time from output callback → frame rendered (ms) */
	renderMs: number;
	/** Standard deviation of inter-frame arrival intervals in the sample window (ms) */
	arrivalJitterMs: number;
	/** Longest gap between consecutive frame arrivals in the sample window (ms) */
	maxGapMs: number;
	/** Count of frames arriving >2x nominal interval after previous (late frames) */
	lateFrames: number;
}
