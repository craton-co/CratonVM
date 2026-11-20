# Moving young generation

The Generational collector uses semispace Cheney evacuation by default.
`DEFAULT_MOVING_YOUNG` in `types/src/flags.rs` owns the default;
`CRATONVM_NO_MOVING_YOUNG=1` disables it. The legacy opt-in spelling is a no-op.

A requested moving cycle still needs complete, rewritable root coverage.
`gc/src/gen_heap.rs::collect_garbage_inner` diverts to a non-moving cycle when
that proof is incomplete; `gc/src/gc_quiescence.rs` records fallback reasons.
A configuration flag being enabled does not prove that a particular cycle moved.

Roots include interpreter frames, compiled frame maps, scoped native handles,
JNI references, registered thread state, and collector-specific remembered sets.
Objects held only by an unrewritable native local cannot safely be evacuated.
See [native handles](native-handle-discipline.md), [precise maps](precise-jit-maps-default.md),
and [Generational GC](generative-gc-architecture.md).
