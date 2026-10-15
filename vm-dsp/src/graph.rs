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
}

impl Graph {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            edges: Vec::new(),
            order: Vec::new(),
            scratch: Vec::new(),
        }
    }

    pub fn add(&mut self, node: Box<dyn AudioNode>) -> NodeId {
        let id = NodeId(self.nodes.len());
        self.nodes.push(Some(node));
        self.order.push(id);
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
                    node.process(ctx, &ins, &mut outs);
                } else {
                    let ins: Vec<&[f32]> =
                        self.scratch[i - 1].iter().map(Vec::as_slice).collect();
                    node.process(ctx, &ins, &mut outs);
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
                    node.process(ctx, &ins, &mut outs);
                } else {
                    let (head, tail) = self.scratch.split_at_mut(i);
                    let ins: Vec<&[f32]> = head[i - 1].iter().map(Vec::as_slice).collect();
                    let mut outs: Vec<&mut [f32]> =
                        tail[0].iter_mut().map(Vec::as_mut_slice).collect();
                    node.process(ctx, &ins, &mut outs);
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
