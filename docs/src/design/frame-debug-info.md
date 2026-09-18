# Procedure frames in debug metadata

An `exec` does not have its own executable MAST node. Its body may be
merged with adjacent basic blocks, and identical executable nodes may be
shared by different source procedures. Execution-node identity, assembly
context names, and changes to FMP are therefore insufficient to recover a
source call stack.

The assembler records `DebugSourceCallFrame` rows on source occurrences.
Each row names a debug function and a half-open operation range. Rows
active at the same operation are ordered from the outermost procedure to
the innermost. A procedure receives its own source root, so a tail `exec`
wrapper does not change the callee's other uses. Nested source trees remain
shared. Basic-block merging offsets the ranges; finalization adjusts them
for batching and padding; static linking remaps their function indices.
Each row also counts inherited inline calls. This lets a debugger attach
inlined caller scopes to their owning physical frame rather than display
them again inside the callee. Dynamic-boundary context depths augment this
count when frames are queried across packages.

Zero-width external source occurrences may carry boundary rows at their
start index. A source-aware forest-restoration continuation retains that
occurrence while the target package executes. These rows describe callers
which remain active across the external boundary.

`ResumeContext::debug_call_frames()` combines the ranges with the active
continuation ancestors. Pending sibling continuations are excluded. A
frame descriptor identifies the package, source occurrence, function,
range start, and continuation depth. Consumers compare descriptors rather
than function names, preserving recursion and consecutive invocations.

The query allocates only when requested by a debugger. Ordinary execution
does not reconstruct frames or emit frame events, and neither MAST hashes
nor VM cycle counts depend on frame metadata. Source-unaware execution
does not allocate a parallel frame stack.

This changes the package debug-info wire format to version 4. Version 3
debug-info payloads are rejected; use a matching toolchain or recompile.
The change must ship at a breaking-release boundary.
