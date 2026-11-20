# Deoptimization and on-stack replacement

The optimizing tier publishes per-PC frame state so a guard failure can resume
interpretation at the trapping bytecode index. `CRATONVM_DEOPT_REAL=0` disables
real-frame deoptimization. Category-2 and floating-point values need explicit
width and register provenance; ambiguous state must decline precise resume.

## Entry and exit

OSR entry consumes a validated mapping from interpreter locals/stack to compiled
locations. Publication refuses inconsistent coordinate spaces. Exit reconstructs
locals, operand stack, monitors, and supported virtual objects without repeating
Java-visible side effects. An unsupported reconstruction must follow an admitted
fallback rather than invent values.

Compiled artifacts and their metadata share a lifetime contract. Epoch guards
are checked before dereferencing potentially superseded metadata. Recompilation
and per-site de-speculation must distinguish a genuine guard failure from an
invalidated artifact and avoid repeated entry/exit livelock.

See [real-frame deoptimization](real-frame-deopt.md), [entry metadata](jit-osr-entry-metadata.md),
[exit/recompile](jit-osr-exit-and-recompile.md), and
[deopt metadata](../jit/deopt-metadata.md). The owning code is `jit/src/deopt.rs`,
`jit/src/ir_lower.rs`, and the VM's JIT helpers and interpreter resume paths.
