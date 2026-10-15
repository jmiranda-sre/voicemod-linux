//! Adaptive per-channel f32 buffers (capture-callback role).
//!
//! Grows on demand, never shrinks mid-stream and never allocates once
//! large enough — the same contract as the reference callback's
//! per-channel resizeable buffers.

#[derive(Debug, Default)]
pub struct ResizableFrameBuffer {
    channels: Vec<Vec<f32>>,
}

impl ResizableFrameBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ensure `channels` lanes with at least `frames` capacity each.
    /// Allocates only when growing.
    pub fn ensure(&mut self, channels: usize, frames: usize) {
        if self.channels.len() != channels {
            self.channels = (0..channels).map(|_| Vec::new()).collect();
        }
        for lane in &mut self.channels {
            if lane.len() < frames {
                lane.resize(frames, 0.0);
            }
        }
    }

    pub fn lane_mut(&mut self, ch: usize) -> &mut [f32] {
        &mut self.channels[ch]
    }

    pub fn channels(&self) -> usize {
        self.channels.len()
    }
}
