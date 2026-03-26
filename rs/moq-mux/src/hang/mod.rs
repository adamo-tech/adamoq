mod container;

pub use container::Legacy;
#[cfg(feature = "mp4")]
pub use container::Media;

pub type Consumer = crate::ordered::Consumer<Legacy>;
pub type Producer = crate::ordered::Producer<Legacy>;
pub use crate::container::Frame;
