//! Node factory: instantiate DSP nodes by string name.
//!
//! Same *role* as the reference `Factory::registerItem` (name →
//! constructor with a context), implemented from scratch.

use super::node::AudioNode;
use std::collections::HashMap;

/// Context handed to every node constructor (rate + channel budget).
#[derive(Debug, Clone, Copy)]
pub struct NodeConstructorContext {
    pub rate: u32,
    pub max_channels: usize,
}

type Ctor = fn(&NodeConstructorContext) -> Box<dyn AudioNode>;

#[derive(Default)]
pub struct NodeFactory {
    items: HashMap<&'static str, Ctor>,
}

impl NodeFactory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a node type under its canonical name.
    pub fn register(&mut self, name: &'static str, ctor: Ctor) {
        self.items.insert(name, ctor);
    }

    /// Instantiate by name. Unknown names mirror
    /// "Unable to find param/node"-style errors at a higher level.
    pub fn create(
        &self,
        name: &str,
        ctx: &NodeConstructorContext,
    ) -> Result<Box<dyn AudioNode>, String> {
        self.items
            .get(name)
            .map(|ctor| ctor(ctx))
            .ok_or_else(|| format!("unknown node type: {name}"))
    }

    pub fn names(&self) -> Vec<&'static str> {
        let mut names: Vec<_> = self.items.keys().copied().collect();
        names.sort_unstable();
        names
    }
}

/// Canonical factory pre-loaded with the MVP node set.
pub fn mvp_factory() -> NodeFactory {
    let mut f = NodeFactory::new();
    f.register("null", |_| Box::new(super::nodes::Null));
    f.register("gain", |_| Box::new(super::nodes::Gain::default()));
    f.register("mute", |_| Box::new(super::nodes::Mute::default()));
    f.register("mix", |_| Box::new(super::nodes::Mix::default()));
    f.register("biquad", |_| Box::new(super::nodes::Biquad::default()));
    f.register("delay", |_| Box::new(super::nodes::Delay::default()));
    f.register("pitch-fft", |_| Box::new(super::nodes::PitchFFT::default()));
    f.register("formant-pitch-shifter", |_| {
        Box::new(super::nodes::FormantPitchShifter::default())
    });
    f.register("reverb", |_| Box::new(super::nodes::Reverb::default()));
    f.register("tube", |_| Box::new(super::nodes::Tube::default()));
    f.register("oscillator", |_| Box::new(super::nodes::Oscillator::default()));
    // Dynamics section (Fase 7 — mic-improvement chains).
    f.register("gate", |_| Box::new(super::dynamics::Gate::default()));
    f.register("compressor", |_| Box::new(super::dynamics::Compressor::default()));
    f.register("limiter", |_| Box::new(super::dynamics::Limiter::default()));
    f.register("de-esser", |_| Box::new(super::dynamics::DeEsser::default()));
    f.register("denoise", |_| Box::new(super::denoise::Denoise::default()));
    f
}
