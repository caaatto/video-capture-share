use parking_lot::Mutex;
use std::sync::Arc;

/// One decoded RGB frame from the capture device.
#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGB8, length = width * height * 3.
    pub rgb: Arc<Vec<u8>>,
    /// Monotonic counter, increments each time the capture thread publishes.
    pub seq: u64,
}

/// Latest-frame slot, single producer many consumer. The capture thread overwrites;
/// readers always see the newest frame, never block the producer, and never queue up.
#[derive(Clone)]
pub struct SharedFrame {
    inner: Arc<Mutex<Option<Frame>>>,
}

impl SharedFrame {
    pub fn new() -> Self {
        Self { inner: Arc::new(Mutex::new(None)) }
    }

    pub fn publish(&self, frame: Frame) {
        *self.inner.lock() = Some(frame);
    }

    pub fn get(&self) -> Option<Frame> {
        self.inner.lock().clone()
    }
}
