//! Node graph: DAG of [`AudioNode`]s executed in insertion order.
//!
//! The reference builds voice chains from presets; this is our
//! clean-room equivalent. Cycles are rejected at build time.

use super::node::{AudioNode, ProcessContext};

/// Opaque node handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub usize);

/// A directed edge: `from` output channel `from_ch` → `to` input `to_ch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edge {
    pub from: NodeId,
    pub from_ch: usize,
    pub to: NodeId,
    pub to_ch: usize,
}

#[derive(Debug)]
pub enum GraphError {
    UnknownNode(NodeId),
    Cycle,
    ChannelMismatch,
}

impl std::fmt::Display for GraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownNode(id) => write!(f, "unknown node: {}", id.0),
            Self::Cycle => write!(f, "graph contains a cycle"),
            Self::ChannelMismatch => write!(f, "channel mismatch on edge"),
        }
    }
}

impl std::error::Error for GraphError {}

pub struct Graph {
    nodes: Vec<Option<Box<dyn AudioNode>>>,
    edges: Vec<Edge>,
    order: Vec<NodeId>,
    /// Intermediate stages: `scratch[i]` holds node `i`'s output lanes.
    /// Sized on demand; never reallocates mid-stream once large enough.
    scratch: Vec<Vec<Vec<f32>>>,
    /// Per-node timing, indexed by `NodeId.0`. Updated in place on the
    /// RT path (no allocation); read from the control plane under lock.
    /// `Instant::now` is vDSO `clock_gettime` (~20ns) — negligible next
    /// to a 960-frame quantum, and the reference tracks
    /// `callback_average_duration_us` the same way.
    telemetry: Vec<NodeTelemetry>,
}

/// Per-node RT cost. Exponential moving average (α = 1/16, same as
/// [`crate::CallbackStats`]) plus a latched peak and call count.
#[derive(Debug, Clone, Copy, Default)]
pub struct NodeTelemetry {
    pub avg_us: f64,
    pub max_us: u64,
    pub calls: u64,
}

impl NodeTelemetry {
    fn record(&mut self, duration_us: u64) {
        const ALPHA: f64 = 1.0 / 16.0;
        self.avg_us += ALPHA * (duration_us as f64 - self.avg_us);
        self.max_us = self.max_us.max(duration_us);
        self.calls += 1;
    }
}

impl Graph {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            edges: Vec::new(),
            order: Vec::new(),
            scratch: Vec::new(),
            telemetry: Vec::new(),
        }
    }

    pub fn add(&mut self, node: Box<dyn AudioNode>) -> NodeId {
        let id = NodeId(self.nodes.len());
        self.nodes.push(Some(node));
        self.order.push(id);
        // Pre-sized here (control plane) so `process` never grows it.
        self.telemetry.push(NodeTelemetry::default());
        id
    }

    /// Connect two nodes. Topological order is insertion order; edges
    /// that would form a cycle are rejected.
    pub fn connect(&mut self, edge: Edge) -> Result<(), GraphError> {
        let (NodeId(a), NodeId(b)) = (edge.from, edge.to);
        if a >= self.nodes.len() || b >= self.nodes.len() {
            return Err(GraphError::UnknownNode(if a >= self.nodes.len() {
                edge.from
            } else {
                edge.to
            }));
        }
        if self.reaches(b, a) {
            return Err(GraphError::Cycle);
        }
        self.edges.push(edge);
        Ok(())
    }

    fn reaches(&self, from: usize, to: usize) -> bool {
        let mut stack = vec![from];
        let mut seen = vec![false; self.nodes.len()];
        while let Some(n) = stack.pop() {
            if n == to {
                return true;
            }
            if seen[n] {
                continue;
            }
            seen[n] = true;
            stack.extend(
                self.edges
                    .iter()
                    .filter(|e| e.from.0 == n)
                    .map(|e| e.to.0),
            );
        }
        false
    }

    pub fn node_mut(&mut self, id: NodeId) -> Option<&mut (dyn AudioNode + 'static)> {
        self.nodes.get_mut(id.0)?.as_deref_mut()
    }

    /// Number of nodes (diagnostic / status reporting).
    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Per-node RT cost snapshot, in insertion order: `(NodeId,
    /// telemetry)`. Control-plane read (allocates); the RT path only
    /// updates the pre-sized `telemetry` vec in place.
    pub fn telemetry(&self) -> Vec<(NodeId, NodeTelemetry)> {
        self.order.iter().map(|id| (*id, self.telemetry[id.0])).collect()
    }

    /// Process the whole chain in insertion order.
    ///
    /// `inputs`/`outputs`: one [`Vec<f32>`] per channel, each holding
    /// exactly `ctx.frames` samples. Intermediate stages live in owned
    /// scratch (grown on demand only). The only per-call allocation is a
    /// handful of slice references; static routing tables land in Fase 3b.
    pub fn process(
        &mut self,
        ctx: &ProcessContext,
        inputs: &[Vec<f32>],
        outputs: &mut [Vec<f32>],
    ) {
        let n = self.order.len();
        if n == 0 || inputs.len() != outputs.len() {
            return;
        }
        let ch = inputs.len();
        if self.scratch.len() != n {
            self.scratch = vec![vec![Vec::new(); ch]; n];
        }
        for stage in &mut self.scratch {
            for lane in stage.iter_mut() {
                if lane.len() < ctx.frames {
                    lane.resize(ctx.frames, 0.0);
                }
            }
        }

        for (i, id) in self.order.clone().into_iter().enumerate() {
            let last = i + 1 == n;
            // No slice views survive across iterations: each iteration
            // re-borrows exactly what it needs (`split_at_mut` when source
            // and destination both live in scratch).
            if last {
                let mut outs: Vec<&mut [f32]> =
                    outputs.iter_mut().map(Vec::as_mut_slice).collect();
                let node = match self.nodes[id.0].as_deref_mut() {
                    Some(node) => node,
                    None => continue,
                };
                if i == 0 {
                    let ins: Vec<&[f32]> = inputs.iter().map(Vec::as_slice).collect();
                    let t0 = std::time::Instant::now();
                    node.process(ctx, &ins, &mut outs);
                    self.telemetry[id.0].record(elapsed_us(t0));
                } else {
                    let ins: Vec<&[f32]> =
                        self.scratch[i - 1].iter().map(Vec::as_slice).collect();
                    let t0 = std::time::Instant::now();
                    node.process(ctx, &ins, &mut outs);
                    self.telemetry[id.0].record(elapsed_us(t0));
                }
            } else {
                let node = match self.nodes[id.0].as_deref_mut() {
                    Some(node) => node,
                    None => continue,
                };
                if i == 0 {
                    let ins: Vec<&[f32]> = inputs.iter().map(Vec::as_slice).collect();
                    let mut outs: Vec<&mut [f32]> =
                        self.scratch[i].iter_mut().map(Vec::as_mut_slice).collect();
                    let t0 = std::time::Instant::now();
                    node.process(ctx, &ins, &mut outs);
                    self.telemetry[id.0].record(elapsed_us(t0));
                } else {
                    let (head, tail) = self.scratch.split_at_mut(i);
                    let ins: Vec<&[f32]> = head[i - 1].iter().map(Vec::as_slice).collect();
                    let mut outs: Vec<&mut [f32]> =
                        tail[0].iter_mut().map(Vec::as_mut_slice).collect();
                    let t0 = std::time::Instant::now();
                    node.process(ctx, &ins, &mut outs);
                    self.telemetry[id.0].record(elapsed_us(t0));
                }
            }
        }
    }
}

impl Default for Graph {
    fn default() -> Self {
        Self::new()
    }
}

/// Saturating µs between `t0` and now (monotonic clock; never panics).
fn elapsed_us(t0: std::time::Instant) -> u64 {
    t0.elapsed().as_micros().min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod graph_tests {
    use super::super::node::ProcessContext;
    use super::*;

    struct Pass;
    impl super::super::node::AudioNode for Pass {
        fn name(&self) -> &'static str {
            "pass"
        }
        fn process(
            &mut self,
            _ctx: &ProcessContext,
            inputs: &[&[f32]],
            outputs: &mut [&mut [f32]],
        ) {
            for (o, i) in outputs.iter_mut().zip(inputs.iter()) {
                o.copy_from_slice(i);
            }
        }
        fn set_param(
            &mut self,
            _name: &str,
            _value: crate::ParamValue,
        ) -> Result<(), super::super::node::ParamError> {
            Ok(())
        }
        fn params(&self) -> Vec<crate::PublicParam> {
            Vec::new()
        }
    }

    fn ctx(frames: usize) -> ProcessContext {
        ProcessContext { rate: 48_000, frames }
    }

    #[test]
    fn telemetry_counts_calls_and_tracks_peak() {
        let mut g = Graph::new();
        assert!(g.is_empty());
        g.add(Box::new(Pass));
        g.add(Box::new(Pass));
        // Wire 0 -> 1 so both stages run (linear order either way).
        g.connect(Edge { from: NodeId(0), from_ch: 0, to: NodeId(1), to_ch: 0 }).unwrap();
        let input = vec![vec![0.25f32; 64]];
        let mut out = vec![vec![0.0f32; 64]];
        for _ in 0..5 {
            g.process(&ctx(64), &input, &mut out);
        }
        assert!((out[0][0] - 0.25).abs() < 1e-6);
        let t = g.telemetry();
        assert_eq!(t.len(), 2);
        for (id, stat) in &t {
            assert_eq!(stat.calls, 5, "node {}", id.0);
            assert!(stat.max_us >= stat.avg_us as u64);
        }
        assert_eq!(t[0].0, NodeId(0));
        assert_eq!(t[1].0, NodeId(1));
    }

    #[test]
    fn empty_graph_reports_no_telemetry() {
        let g = Graph::new();
        assert!(g.telemetry().is_empty());
    }
}
