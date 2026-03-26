use std::sync::Arc;

use bytes::Buf;

use crate::error::MoqError;
use crate::ffi::Task;
use crate::media::*;

#[derive(Clone, uniffi::Object)]
pub struct MoqBroadcastConsumer {
	inner: moq_lite::BroadcastConsumer,
}

impl MoqBroadcastConsumer {
	pub(crate) fn new(inner: moq_lite::BroadcastConsumer) -> Self {
		Self { inner }
	}
}

#[derive(uniffi::Object)]
pub struct MoqCatalogConsumer {
	task: Task<Catalog>,
}

struct Catalog {
	inner: hang::CatalogConsumer,
}

impl Catalog {
	async fn next(&mut self) -> Result<Option<MoqCatalog>, MoqError> {
		match self.inner.next().await {
			Ok(Some(catalog)) => Ok(Some(convert_catalog(&catalog))),
			Ok(None) => Ok(None),
			Err(e) => Err(e.into()),
		}
	}
}

#[derive(uniffi::Object)]
pub struct MoqMediaConsumer {
	task: Task<Media>,
}

enum MediaInner {
	Legacy(moq_mux::hang::Consumer),
	Media(moq_mux::ordered::Consumer<moq_mux::hang::Media>),
}

struct Media {
	inner: MediaInner,
}

impl Media {
	async fn next(&mut self) -> Result<Option<MoqFrame>, MoqError> {
		let frame = match &mut self.inner {
			MediaInner::Legacy(c) => c.read().await?,
			MediaInner::Media(c) => c.read().await.map_err(|e| MoqError::Codec(e.to_string()))?,
		};

		let Some(frame) = frame else {
			return Ok(None);
		};

		let timestamp_us: u64 = frame
			.timestamp
			.as_micros()
			.try_into()
			.map_err(|_| MoqError::Codec("timestamp overflow".into()))?;

		let mut buf = frame.payload;
		let payload = buf.copy_to_bytes(buf.remaining()).to_vec();

		Ok(Some(MoqFrame {
			payload,
			timestamp_us,
			keyframe: frame.keyframe,
		}))
	}
}

// ---- Broadcast ----

#[uniffi::export]
impl MoqBroadcastConsumer {
	/// Subscribe to the catalog for this broadcast.
	pub fn subscribe_catalog(&self) -> Result<Arc<MoqCatalogConsumer>, MoqError> {
		let _guard = crate::ffi::RUNTIME.enter();
		let track = self.inner.subscribe_track(&hang::catalog::Catalog::default_track())?;
		let consumer = hang::CatalogConsumer::from(track);
		Ok(Arc::new(MoqCatalogConsumer {
			task: Task::new(Catalog { inner: consumer }),
		}))
	}

	/// Subscribe to a video track by name, delivering frames in decode order.
	///
	/// The container format (legacy or CMAF) is automatically demuxed based on the
	/// catalog config, returning raw codec bitstream frames.
	///
	/// `config` is the video track configuration from the catalog.
	/// `max_latency_ms` controls the maximum buffering before skipping a GoP.
	pub fn subscribe_video(
		&self,
		name: String,
		config: MoqVideo,
		max_latency_ms: u64,
	) -> Result<Arc<MoqMediaConsumer>, MoqError> {
		let _guard = crate::ffi::RUNTIME.enter();
		let track = self.inner.subscribe_track(&moq_lite::Track { name, priority: 0 })?;
		let video_config = hang::catalog::VideoConfig::try_from(config)?;
		let latency = std::time::Duration::from_millis(max_latency_ms);
		let consumer =
			moq_mux::ordered::Consumer::new(track, moq_mux::hang::Media::Video(video_config)).with_latency(latency);
		Ok(Arc::new(MoqMediaConsumer {
			task: Task::new(Media {
				inner: MediaInner::Media(consumer),
			}),
		}))
	}

	/// Subscribe to an audio track by name, delivering frames in decode order.
	///
	/// The container format (legacy or CMAF) is automatically demuxed based on the
	/// catalog config, returning raw codec bitstream frames.
	///
	/// `config` is the audio track configuration from the catalog.
	/// `max_latency_ms` controls the maximum buffering before skipping a GoP.
	pub fn subscribe_audio(
		&self,
		name: String,
		config: MoqAudio,
		max_latency_ms: u64,
	) -> Result<Arc<MoqMediaConsumer>, MoqError> {
		let _guard = crate::ffi::RUNTIME.enter();
		let track = self.inner.subscribe_track(&moq_lite::Track { name, priority: 0 })?;
		let audio_config = hang::catalog::AudioConfig::try_from(config)?;
		let latency = std::time::Duration::from_millis(max_latency_ms);
		let consumer =
			moq_mux::ordered::Consumer::new(track, moq_mux::hang::Media::Audio(audio_config)).with_latency(latency);
		Ok(Arc::new(MoqMediaConsumer {
			task: Task::new(Media {
				inner: MediaInner::Media(consumer),
			}),
		}))
	}
}

#[uniffi::export]
impl MoqBroadcastConsumer {
	/// Subscribe to a media track by name using the legacy container format.
	///
	/// Deprecated: Use `subscribe_video` or `subscribe_audio` instead for CMAF support.
	pub fn subscribe_media(&self, name: String, max_latency_ms: u64) -> Result<Arc<MoqMediaConsumer>, MoqError> {
		let _guard = crate::ffi::RUNTIME.enter();
		let track = self.inner.subscribe_track(&moq_lite::Track { name, priority: 0 })?;
		let latency = std::time::Duration::from_millis(max_latency_ms);
		let consumer = moq_mux::hang::Consumer::new(track, moq_mux::hang::Legacy).with_latency(latency);
		Ok(Arc::new(MoqMediaConsumer {
			task: Task::new(Media {
				inner: MediaInner::Legacy(consumer),
			}),
		}))
	}
}

// ---- Catalog Consumer ----

#[uniffi::export]
impl MoqCatalogConsumer {
	/// Get the next catalog update. Returns `None` when the track ends or is closed.
	pub async fn next(&self) -> Result<Option<MoqCatalog>, MoqError> {
		self.task.run(|mut state| async move { state.next().await }).await
	}

	/// Cancel all current and future `next()` calls.
	pub fn cancel(&self) {
		self.task.cancel();
	}
}

// ---- Media Consumer ----

#[uniffi::export]
impl MoqMediaConsumer {
	/// Get the next frame. Returns `None` when the track ends or is closed.
	pub async fn next(&self) -> Result<Option<MoqFrame>, MoqError> {
		self.task.run(|mut state| async move { state.next().await }).await
	}

	/// Cancel all current and future `next()` calls.
	pub fn cancel(&self) {
		self.task.cancel();
	}
}
