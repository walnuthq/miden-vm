//! This module defines items relevant to controlling execution stopping conditions.

use alloc::{sync::Arc, vec, vec::Vec};
use core::ops::ControlFlow;

use miden_core::{mast::MastForest, program::KernelDescriptor};
use miden_mast_package::debug_info::{
    DebugFunctionIdx, DebugFunctionInfo, DebugSourceNodeId, PackageDebugInfo,
};

use crate::{
    ExecutionError, FastProcessor, SourceInlineCallContext, Stopper,
    continuation_stack::{Continuation, ContinuationStack},
};

// RESUME CONTEXT
// ===============================================================================================

/// The context required to resume execution of a program from the last point at which it was
/// stopped.
#[derive(Debug)]
pub struct ResumeContext {
    pub(crate) current_forest: Arc<MastForest>,
    pub(crate) continuation_stack: ContinuationStack<Arc<MastForest>>,
    pub(crate) kernel: KernelDescriptor,
    pub(crate) package_debug_info: Option<Arc<PackageDebugInfo>>,
    pub(crate) inline_call_contexts: Vec<Option<SourceInlineCallContext>>,
}

/// A source-level physical call frame resolved from package debug metadata.
#[derive(Clone, Debug)]
pub struct DebugCallFrame {
    debug_info: Arc<PackageDebugInfo>,
    function_idx: DebugFunctionIdx,
    source_node_id: DebugSourceNodeId,
    range_start: u32,
    continuation_depth: usize,
    inherited_inline_calls: usize,
}

impl DebugCallFrame {
    pub fn function_idx(&self) -> DebugFunctionIdx {
        self.function_idx
    }

    pub fn function(&self) -> &DebugFunctionInfo {
        &self.debug_info[self.function_idx]
    }

    pub fn debug_info(&self) -> &PackageDebugInfo {
        &self.debug_info
    }

    /// Number of inline frames at the end of the active inline chain owned by callers.
    pub fn inherited_inline_calls(&self) -> usize {
        self.inherited_inline_calls
    }

    /// Whether these descriptors refer to the same active invocation.
    pub fn is_same_frame(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.debug_info, &other.debug_info)
            && self.function_idx == other.function_idx
            && self.source_node_id == other.source_node_id
            && self.range_start == other.range_start
            && self.continuation_depth == other.continuation_depth
    }
}
impl ResumeContext {
    /// Resolves the active source-level physical call chain for the next operation.
    ///
    /// This is a query-only operation over source-aware continuation state and package debug
    /// metadata. It does not add work to the processor's normal execution path.
    pub fn debug_call_frames(&self) -> Vec<DebugCallFrame> {
        let continuations = self.continuation_stack.iter_with_source_node_ids().collect::<Vec<_>>();
        let next_count = self.continuation_stack.iter_continuations_for_next_clock().count();
        let next_start = continuations.len().saturating_sub(next_count);

        let mut debug_info_by_continuation = vec![None; continuations.len()];
        let mut active_debug_info = self.package_debug_info.clone();
        let mut inline_depth_by_continuation = vec![0; continuations.len()];
        let mut inline_depth = self.inline_call_contexts.len();
        for (index, (continuation, _)) in continuations.iter().enumerate().rev() {
            if let Continuation::EnterForest {
                package_debug_info, inline_context_depth, ..
            } = continuation
            {
                active_debug_info = package_debug_info.clone();
                inline_depth = *inline_context_depth;
            }
            if matches!(continuation, Continuation::FinishDyn(_)) {
                inline_depth = inline_depth.saturating_sub(1);
            }
            debug_info_by_continuation[index] = active_debug_info.clone();
            inline_depth_by_continuation[index] = self.inline_call_contexts
                [..inline_depth.min(self.inline_call_contexts.len())]
                .iter()
                .filter_map(Option::as_ref)
                .map(|context| context.inline_calls().count())
                .sum::<usize>();
        }

        let mut frames = Vec::new();
        for (index, (continuation, source_node_id)) in continuations.iter().enumerate() {
            let active_ancestor = matches!(
                continuation,
                Continuation::FinishJoin(_)
                    | Continuation::FinishSplit(_)
                    | Continuation::FinishLoop(_)
                    | Continuation::FinishCall(_)
                    | Continuation::FinishDyn(_)
                    | Continuation::EnterForest { .. }
            );
            if index > next_start || (!active_ancestor && index != next_start) {
                continue;
            }
            let (Some(debug_info), Some(source_node_id)) =
                (debug_info_by_continuation[index].as_ref(), source_node_id)
            else {
                continue;
            };
            let Some(source_node) = debug_info.source_node(*source_node_id) else {
                continue;
            };
            let op_idx = if index == next_start {
                match continuation {
                    Continuation::ResumeBasicBlock { node_id, batch_index, op_idx_in_batch } => {
                        let block = self.current_forest[*node_id].unwrap_basic_block();
                        let offset = block
                            .op_batches()
                            .iter()
                            .take(*batch_index)
                            .map(|batch| batch.ops().len())
                            .sum::<usize>();
                        (offset + op_idx_in_batch) as u32
                    },
                    Continuation::Respan { node_id, batch_index } => {
                        let block = self.current_forest[*node_id].unwrap_basic_block();
                        block
                            .op_batches()
                            .iter()
                            .take(*batch_index)
                            .map(|batch| batch.ops().len() as u32)
                            .sum()
                    },
                    Continuation::FinishBasicBlock(node_id) => self.current_forest[*node_id]
                        .unwrap_basic_block()
                        .num_operations()
                        .saturating_sub(1),
                    _ => source_node.op_start,
                }
            } else {
                source_node.op_start
            };
            for row in &source_node.call_frames {
                if (row.op_start <= op_idx && op_idx < row.op_end)
                    || (row.op_start == row.op_end && op_idx == row.op_start)
                {
                    append_frame(
                        &mut frames,
                        debug_info,
                        row.function_idx,
                        *source_node_id,
                        row.op_start,
                        index,
                        row.inherited_inline_calls as usize + inline_depth_by_continuation[index],
                    );
                }
            }
        }

        frames
    }
    /// Returns a reference to the continuation stack.
    pub fn continuation_stack(&self) -> &ContinuationStack<Arc<MastForest>> {
        &self.continuation_stack
    }

    /// Returns a reference to the MAST forest being currently executed.
    pub fn current_forest(&self) -> &Arc<MastForest> {
        &self.current_forest
    }

    /// Returns a reference to the debug info associated with the current forest, if available
    pub fn debug_info(&self) -> Option<Arc<PackageDebugInfo>> {
        self.package_debug_info.clone()
    }

    /// Returns the source/debug occurrence associated with the next continuation, if available.
    pub fn next_source_node_id(&self) -> Option<DebugSourceNodeId> {
        self.continuation_stack
            .peek_continuation_with_source_node_id()
            .and_then(|(_, source_node_id)| source_node_id)
    }

    /// Returns dynamic-boundary inline contexts active for the next operation, ordered from the
    /// innermost boundary to the outermost.
    pub fn inherited_inline_call_contexts(&self) -> impl Iterator<Item = &SourceInlineCallContext> {
        let effective_depth = self.continuation_stack.iter_continuations_for_next_clock().fold(
            self.inline_call_contexts.len(),
            |depth, continuation| match continuation {
                Continuation::EnterForest { inline_context_depth, .. } => *inline_context_depth,
                Continuation::FinishDyn(_) => depth.saturating_sub(1),
                _ => depth,
            },
        );
        self.inline_call_contexts[..effective_depth.min(self.inline_call_contexts.len())]
            .iter()
            .rev()
            .filter_map(Option::as_ref)
    }

    /// Returns a reference to the kernel being currently executed.
    pub fn kernel(&self) -> &KernelDescriptor {
        &self.kernel
    }
}

fn append_frame(
    frames: &mut Vec<DebugCallFrame>,
    debug_info: &Arc<PackageDebugInfo>,
    function_idx: DebugFunctionIdx,
    source_node_id: DebugSourceNodeId,
    range_start: u32,
    continuation_depth: usize,
    inherited_inline_calls: usize,
) {
    frames.push(DebugCallFrame {
        debug_info: Arc::clone(debug_info),
        function_idx,
        source_node_id,
        range_start,
        continuation_depth,
        inherited_inline_calls,
    });
}
// STOPPERS
// ===============================================================================================

/// A [`Stopper`] that never stops execution (except for returning an error when the maximum cycle
/// count is exceeded).
pub struct NeverStopper;

impl Stopper for NeverStopper {
    type Processor = FastProcessor;
    type Forest = Arc<MastForest>;

    #[inline(always)]
    fn should_stop(
        &self,
        processor: &FastProcessor,
        continuation_stack: &ContinuationStack<Arc<MastForest>>,
        _continuation_after_stop: impl FnOnce() -> Option<(
            Continuation<Arc<MastForest>>,
            Option<DebugSourceNodeId>,
        )>,
    ) -> ControlFlow<BreakReason<Arc<MastForest>>> {
        check_if_max_cycles_exceeded(processor)?;
        check_if_continuation_stack_too_large(processor, continuation_stack)
    }
}

/// A [`Stopper`] that always stops execution after each single step. An error is returned if the
/// maximum cycle count is exceeded.
pub struct StepStopper;

impl Stopper for StepStopper {
    type Processor = FastProcessor;
    type Forest = Arc<MastForest>;

    #[inline(always)]
    fn should_stop(
        &self,
        processor: &FastProcessor,
        continuation_stack: &ContinuationStack<Arc<MastForest>>,
        continuation_after_stop: impl FnOnce() -> Option<(
            Continuation<Arc<MastForest>>,
            Option<DebugSourceNodeId>,
        )>,
    ) -> ControlFlow<BreakReason<Arc<MastForest>>> {
        check_if_max_cycles_exceeded(processor)?;
        check_if_continuation_stack_too_large(processor, continuation_stack)?;

        ControlFlow::Break(BreakReason::Stopped(continuation_after_stop()))
    }
}

/// Checks if the maximum cycle count has been exceeded, returning a `BreakReason::Err` if so.
#[inline(always)]
fn check_if_max_cycles_exceeded<F>(processor: &FastProcessor) -> ControlFlow<BreakReason<F>> {
    if processor.clk > processor.options.max_cycles() as usize {
        ControlFlow::Break(BreakReason::Err(ExecutionError::CycleLimitExceeded(
            processor.options.max_cycles(),
        )))
    } else {
        ControlFlow::Continue(())
    }
}

/// Checks if the continuation stack size exceeds the maximum allowed, returning a
/// `BreakReason::Err` if so.
#[inline(always)]
fn check_if_continuation_stack_too_large<F>(
    processor: &FastProcessor,
    continuation_stack: &ContinuationStack<F>,
) -> ControlFlow<BreakReason<F>> {
    if continuation_stack.len() > processor.options.max_num_continuations() {
        ControlFlow::Break(BreakReason::Err(ExecutionError::Internal(
            "continuation stack size exceeded the allowed maximum",
        )))
    } else {
        ControlFlow::Continue(())
    }
}

// BREAK REASON
// ===============================================================================================

/// The reason why execution was interrupted.
#[derive(Debug)]
pub enum BreakReason<F> {
    /// An execution error occurred
    Err(ExecutionError),
    /// Execution was stopped by a [`Stopper`]. Provides the continuation to add to the continuation
    /// stack before returning, if any. The mental model to have in mind when choosing the
    /// continuation to add on a call to `FastProcessor::increment_clk()` is:
    ///
    /// "If execution is stopped here, does the current continuation stack properly encode the next
    /// step of execution?"
    ///
    /// If yes, then `None` should be returned. If not, then the continuation that runs the next
    /// step in `FastProcessor::execute_impl()` should be returned.
    Stopped(Option<(Continuation<F>, Option<DebugSourceNodeId>)>),
}
