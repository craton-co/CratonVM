# JUL GC-stress nested-native ClassCastException — FIXED

**Status: FIXED, 2026-07-21** (4th session). Continues (and closes) the
"deepest residual" split off from
`dohead-post-fix-sporadic-residuals-FIXED.md` — the JUL/`logmanager.rs`
family's last survivor after the six pin fixes merged through `ac04b3ea7`.
The third session's findings doc (nested-native root-scanning hypothesis,
ALTRACE instrumentation, A/B bypass of `create_string_uninterned_gc_safe`'s
proactive GC) fed directly into this one.

## Symptom

`JulGcStressRepro.java` (at `/data/data/` on the Azure host): 2000
`Logger.logp()` calls through a custom `Handler` doing
`received.add(record.getMessage())` under
`CRATONVM_DBG_GC_STRESS=4096..524288`. Verification reads threw
`java.lang.ClassCastException: java.lang.Object cannot be cast to
java.lang.String`; other indices returned wrong-content strings or nulls;
raw dumps showed literal foreign `byte[]`/bare-`Object` instances from the
concurrently-growing `garbage` list. 100% deterministic, `--nojit`
identical.

## Root cause (proven, single line of causality)

A **stale-Rust-local reuse after a GC-capable call** in
`publish_to_jul_handlers_src` (`native-builtins/src/logmanager.rs`):

1. `level`/`message` are read from their pins once, at the top of the
   closure.
2. `ctx.new_object_initialized("java/util/logging/LogRecord", …)` runs the
   LogRecord `<init>` **native** (phases_early), which materializes a
   `java/time/Instant` — an allocation that can (and, under GC stress,
   does) trigger a moving young GC. The address-watch probe caught GC#42
   firing exactly inside `Instant.create` with the repro's message string
   `0x201070007b0` rooted (via its pin) and evacuated to `0x200c2c00048`.
3. Back in `publish_to_jul_handlers_src`, the follow-up field writes
   `set_field_by_name(record, "level"/"message", …)` / `set_field(record,
   4, …)` used the **pre-GC locals** — storing the condemned from-space
   address right back over the field the collector had just fixed up while
   evacuating the record itself.
4. `LogRecord.getMessage()` resolves to phases_early's `lr_get` — a **plain
   field read** (NOT `native_jul_log_record_get_message`; see "shadowed
   registration" below) — so it faithfully returns the poison. Young space
   is bump-reset and immediately reused after the stress GC, so the stale
   address reads as a zero-header object (→ CP-fallback `java/lang/Object`
   → the checkcast), a later different string (wrong content), a foreign
   `byte[]` from `garbage`, or zeroes (null).

Why the earlier hypotheses missed: `set_field`/`set_field_by_name` are
direct heap writes with **no healing** — unlike `invoke_virtual`, whose
entry barrier (`load_and_forward` in `safe_native_call_impl`) silently
heals forwarded args, which is why the `setMessage` invoke on the same
stale local never showed damage. GC root scanning (`collect_roots`),
frame rewriting (`update_all_roots`), the write barrier in
`gen_heap::set_field`, and the return-value healing were all verified
correct along the way — the frame-liveness/nested-root-scan hypothesis
from sessions 1–3 is affirmatively ruled out.

## The fix

`native-builtins/src/logmanager.rs`, `publish_to_jul_handlers_src`:
re-derive `level` and `message` from their pins immediately after
`new_object_initialized` returns, before any raw field write. Two lines +
comment; same idiom as the function's existing post-`setMessage` refresh.

Verified: `JulGcStressRepro` REPRO_PASS with 0 mismatches at stress 4096 /
16384 / 65536 / 262144 / 524288, plus `--nojit`, plus `GcCorruptBisect5`
(single-read variant) and `GcCorruptBisect6` (raw-type dump: all
`String`, no foreign objects) — every previously-failing shape.

## Also fixed in the same change

- **Unbounded `log_record_messages` growth**: the identity-keyed delivery
  path (the ONLY one that fires for a real `Logger.getLogger(...)` logger)
  inserted one side-table entry per log call with no eviction — the
  4096-entry retention cap lived only inside the name-keyed sibling's
  handler loop, which is empty for real loggers. Mirrored the cap into
  `publish_to_jul_handlers_src`.
- Removed the concluded `CRATONVM_DBG_SKIP_STRINGGC` A/B experiment from
  `create_string_uninterned_gc_safe` (it proved the proactive string-GC is
  NOT the trigger; keeping a behavior-changing env gate around was pure
  risk).
- Kept (committed) the reusable env-gated diagnostics built for the hunt:
  `CRATONVM_DBG_ALTRACE` (ArrayList add/get/grow tracing with class+string
  resolution in `native-collections`, per-GC move-count line, object-
  returning-native trace in `safe_native_call_impl`) and
  `CRATONVM_DBG_WATCHADDR=<hex>` (per-GC rootedness + moved-from/to
  tracking for one address in `collect_roots`/`update_all_roots`). The
  combination is what cracked this: watch showed "rooted AND moved", ADD
  showed "stale at write", NRET showed "stale already at native return",
  which pinned the corruption window to the ctor call.

## Known leftovers (documented, deliberately not restructured here)

- **Shadowed registration**: `logmanager.rs` registers
  `native_jul_log_record_get_message` for `LogRecord.getMessage`, but
  phases_early's `lr_get` field-getter is what actually serves dispatch —
  the logmanager registration (and the `0703f2571` gc_safe routing fix
  inside it) is dead code on this path. Same latent-footgun family as the
  dual name-keyed/identity-keyed handler registries (finding #8 of the
  session-3 doc): two registries doing the same job, one silently dead for
  real-JDK-shaped loggers. A consolidation pass should collapse both
  pairs; out of scope for this fix.
- The diagnostic worktree `/data/data/wt-julgc-checkcast-20260721` (session
  3's ALTRACE + SKIP_STRINGGC uncommitted diffs) is superseded by this
  change and can be deleted.
