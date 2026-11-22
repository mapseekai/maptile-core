mod compile;
mod read;
#[cfg(test)]
mod tests;

use std::sync::Arc;

pub use composite_mvt::DuplicateLayer;
use composite_mvt::MvtComposer;

use super::{SourceDescriptor, lifecycle::SourceSlot};
pub(super) use compile::compile_graph;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MemberFailurePolicy {
    #[default]
    FailFast,
    SkipTransient,
}

#[derive(Clone, Debug)]
pub struct CompositeDeclaration {
    pub id: Arc<str>,
    pub members: Vec<Arc<str>>,
    pub lazy: bool,
    pub duplicate_layer: DuplicateLayer,
    pub failure_policy: MemberFailurePolicy,
}

#[derive(Clone, Copy, Debug)]
pub struct CompositeLimits {
    pub max_depth: usize,
    pub max_leaves: usize,
    /// Maximum concurrent leaf reads in one request, including layer validation.
    pub max_concurrency: usize,
    pub max_compressed_bytes: usize,
    pub max_decoded_bytes: usize,
    /// Maximum total byte reservations for encoded inputs, decoded payloads and composed output.
    /// Decoding rechecks admission before retaining its output. Codec state and temporary
    /// parser allocations use additional memory.
    pub max_work_bytes: usize,
}
impl Default for CompositeLimits {
    fn default() -> Self {
        Self {
            max_depth: 64,
            max_leaves: 128,
            max_concurrency: 8,
            max_compressed_bytes: 16 * 1024 * 1024,
            max_decoded_bytes: 16 * 1024 * 1024,
            max_work_bytes: 64 * 1024 * 1024,
        }
    }
}

#[derive(Clone)]
enum Member {
    Source(Arc<SourceSlot>),
    Composite(Arc<CompiledComposite>),
}
impl Member {
    fn descriptor(&self) -> &SourceDescriptor {
        match self {
            Self::Source(s) => s.descriptor(),
            Self::Composite(c) => c.descriptor(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct CompiledComposite {
    descriptor: SourceDescriptor,
    leaves: Box<[Arc<SourceSlot>]>,
    members: Box<[Member]>,
    composer: Option<Arc<MvtComposer>>,
    policy: MemberFailurePolicy,
}
impl CompiledComposite {
    pub(super) fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }
    pub(super) fn leaves(&self) -> &[Arc<SourceSlot>] {
        &self.leaves
    }
}
