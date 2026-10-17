//! `vm-dsp`: N-channel-native DSP node graph (clean-room).
//!
//! Mirrors the ledger (Fase 2, item ②) with original code throughout:
//! - [`node::AudioNode`]: per-node processing (`process`) + named params.
//! - [`factory::NodeFactory`]: instantiate by string name
//!   (same role as the reference `registerItem`, own implementation).
//! - [`param`]: static values + modulation sources (LFO/random/transition/
//!   trigger/sequence) as first-class citizens.
//! - [`buffer::ResizableFrameBuffer`]: adaptive per-channel f32 buffers,
//!   same role as the reference capture-callback buffers.
//! - [`graph::Graph`]: node DAG executed in topological order.
//!
//! Unlike the reference (mono chains replicated per channel), every node
//! here is N-channel native.

pub mod buffer;
pub mod broadcast;
pub mod denoise;
#[cfg(feature = "neural")]
pub mod deepfilter;
pub mod musical;
pub mod neural;
pub mod room;
pub mod dynamics;
pub mod factory;
pub mod graph;
pub mod node;
pub mod nodes;
pub mod param;
pub mod spectral;

pub use buffer::ResizableFrameBuffer;
pub use factory::{NodeConstructorContext, NodeFactory};
pub use graph::{Edge, Graph, NodeId, NodeTelemetry};
pub use node::{AudioNode, ParamError, ProcessContext};
pub use nodes::{biquad_coeffs, biquad_magnitude_db, BqType};
pub use param::{Modulation, ParamValue, PublicParam, RangedParam};
