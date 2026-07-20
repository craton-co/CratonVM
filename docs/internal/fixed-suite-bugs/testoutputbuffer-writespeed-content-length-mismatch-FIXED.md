# TestOutputBuffer.testWriteSpeed — deterministic content-length mismatch (FIXED)

**Status: FIXED 2026-07-20**, branch `fix/tomcat-writespeed-clenmismatch-20260720`.
Originally opened same-day in `docs/known-issues/tomcat-08-07/` while sweeping
the Tomcat connector package for RBC.6-related throughput residuals; this was
a separate, unrelated bug. Root-caused and fixed to completion in a follow-up
session — three independent, compounding JIT defects, all in the x86-64 OSR
back-end (`jit/src/x64.rs`).

## Symptom (recap)

`org.apache.catalina.connector.TestOutputBuffer.testWriteSpeed` failed
deterministically (3/3 runs) with:

```
java.lang.AssertionError: expected:<100000> but was:<382000>
```

The mismatching request was always the **very first** one in the test
(`i=1`, unbuffered — `writeCount=100000`, `writeString` a single `'x'`
character), and the response body was 382000 copies of `'x'` — i.e. the
write loop simply ran too many iterations, not stray/duplicated data from a
different request. `--nojit` never reproduced it; `CRATONVM_JIT_OSR=0`
(disabling back-edge On-Stack-Replacement) also fixed it — isolating the bug
to OSR.

## Root cause (three compounding bugs)

`WritingServlet.doGet`'s loop (`for (int i = 0; i < writeCount; i++)
w.write(writeString);`) is hot enough (100,000 back-edges) to trigger OSR
well before it naturally completes. The OSR-compiled code runs the **entire
rest of the loop for real** (genuine `Writer.write` calls, genuine bytes
sent) and then continues into the method's tail — `System.out.println(...)`,
which javac compiles as an `invokedynamic` call to
`StringConcatFactory.makeConcatWithConstants`.

CratonVM's x64 backend never actually JIT-implements `invokedynamic`: every
`0xba` site unconditionally traps back to the interpreter
(`DeoptReason::UnreachedCode`), on the (usually correct) assumption that indy
sites are rare/dead code (e.g. assert-message string building). For a method
reached via OSR, this trap needs to **resume precisely at the trap's bci**,
because the loop's writes already happened for real — falling back to "just
re-interpret from the OSR entry point" would re-execute the whole loop,
duplicating its output. That precise-resume path exists
(`transfer_osr_exit_into_live_frame`), but it silently failed for this method
and fell back to the unsound "safe reject" path, due to three separate bugs
in how the deopt *snapshot* — the reconstructed locals/operand-stack state at
the trap bci — was built:

1. **Locals: `i` misclassified as unmappable instead of dead.**
   `WritingServlet.doGet`'s local slot 7 holds the loop counter `i` during
   the loop, then gets reused (`lstore 7`) for `lastRunNano` after it. The
   snapshot's per-local classifier (`classify_local_kinds`) correctly marks
   this reused slot `Ambiguous`/`Unsupported` — safe for the LOOP-HEADER OSR
   entry point, where the slot's value genuinely matters — but the deopt
   snapshot for the *later* `invokedynamic` trap used the exact same
   "Unsupported → reject the whole snapshot" rule, even though `i` is
   provably **dead** at that bci (nothing reads it again). One dead,
   undecodable local was enough to veto the entire precise-resume transfer.

   Fixed by adding real per-bci liveness analysis
   (`regalloc::live_locals_per_pc`, a backward dataflow walk reusing the
   register allocator's existing block CFG/gen-kill machinery) and using it
   in `build_and_record_deopt_point`: an `Unsupported` **non-oop** local that
   is proven dead at the snapshot's bci is now encoded as `Undefined` (a safe
   "value never read" placeholder, the same encoding already used for dead
   cat-2 high-halves) instead of vetoing the snapshot. Oop-typed locals are
   left on the existing conservative path (unchanged) since a wrong
   placeholder there risks a GC-root hazard rather than just an unnecessary
   rejection.

2. **Operand stack: a value pushed before a branch loses its oop-mark at the merge.**
   The concat's `println` receiver (`System.out`, pushed via `getstatic`
   *before* an `if (useBufferStr == null) "n" else "y"` ternary used to
   compute one of the concat's arguments) sits on the operand stack across
   that branch. The x64 backend's "dead code becomes live again at a branch
   target" reconstruction (hit here because the `if`/`else`'s `goto` makes
   the `else` arm's textually-following code "dead" until the branch target
   is reached) rebuilt the operand stack's oop-mark vector as all-`false`,
   reasoning that "the merge target's own bytecode will re-tag any slot that
   genuinely holds a reference as it re-executes the producing instruction."
   That's true only for values *produced* between the branch and the merge —
   `System.out` was produced well before the branch and is simply carried
   across untouched, so its oop-mark was silently and permanently zeroed for
   the rest of the compiled method. A wrongly-non-oop `getstatic System.out`
   sitting directly under the `invokedynamic` trap's operand stack is exactly
   the shape the trap's own snapshot needs to decode.

   Fixed by recording the operand-stack oop-mark vector alongside the
   existing stack-depth bookkeeping at every branch-target-recording site
   (`record_branch_target_depth`, replacing ~11 duplicated
   `branch_target_stack_depth.entry(...).or_insert(...)` call sites), and
   using the recorded marks — instead of a blind all-`false` reset — when
   the dead-code merge reconstruction fires.

3. **Operand stack: a coarse per-method gate marks the indy call's own int/long arguments `Unsupported`.**
   The snapshot's operand-stack decoder has no per-entry width source (only
   `local`s get typed, via `classify_local_kinds`) — instead a method-level
   `uses_long_float_double` gate marks *every* non-oop stack slot
   `Unsupported` once the method touches a `long`/`float`/`double`
   *anywhere*, since a `long` argument could otherwise be mistyped as a
   truncated `int`. `WritingServlet.doGet` uses `long start`/`lastRunNano`
   for timing, so this gate fired, blocking exactly the `int` (`writeString
   .length()`) and `long` (`lastRunNano`) arguments the concat's own
   `invokedynamic` needs.

   Fixed narrowly at the one place stack shape actually *is* known
   precisely without a full abstract-stack type simulation: an
   `invokedynamic` call site's own bootstrap descriptor fixes exactly how
   many arguments are live directly beneath it and their types, in order.
   Added `indy_arg_type_tags` (a per-argument JVM type tag list, threaded
   through `indy_info` from both `interpreter.rs` construction sites and the
   IR-scan helper in `jit/src/lib.rs`) and used it in
   `build_and_record_deopt_point` to precisely type the top
   `arg_type_tags.len()` operand-stack entries at an indy trap bci, ahead of
   the coarse `wide_fp` fallback for everything else.

All three had to be fixed together — any one alone still left the snapshot
rejecting (confirmed incrementally: fixing only #1 moved the failure from
"unmappable local" to "unmappable stack slot" at index 0; fixing #1+#2 moved
it to "unmappable stack slot" at the remaining int/long argument indices;
only #1+#2+#3 together make the transfer succeed).

## Verification

- New unit tests: `jit/src/regalloc.rs` (`live_locals_per_pc_*`, 2 tests),
  `jit/src/lib.rs` (`indy_arg_type_tags_*`, 4 tests). Full `cratonvm-jit`
  unit suite: 921/921 pass (was 917 pre-fix). `cratonvm-vm` unit suite:
  2232/2240 pass — the 8 failures are pre-existing on plain `dev`
  (`99a4c4108`, confirmed via `git stash`), unrelated to this change.
- `TestOutputBuffer` (all 3 test methods, unmodified): 11 consecutive clean
  runs (`OK (3 tests)`), vs. deterministic 3/3 failure before the fix.
- Full `org.apache.catalina.connector` package (17 test classes, 347 tests):
  `OK (347 tests)` — no regression from the OSR-exit/branch-merge/indy-arg
  changes across a broad, realistic mix of HTTP request/response, I/O, and
  string-processing code shapes.

## Files changed

- `jit/src/regalloc.rs` — new `live_locals_per_pc` (per-bci local liveness).
- `jit/src/lib.rs` — new `indy_arg_type_tags`; `indy_info`'s construction
  site gains the tag list.
- `jit/src/x64.rs` — `local_liveness` field + dead-local snapshot fix;
  `branch_target_stack_oop_marks` field + `record_branch_target_depth`
  helper + dead-code-merge reconstruction fix; `indy_stack_arg_types` field
  + indy-arg-precise-typing fix in `build_and_record_deopt_point`.
- `vm/src/runtime/interpreter.rs` — both `indy_info` construction sites
  (method-entry and OSR compile paths) populate the new per-arg type tags.

## Not chased further

The dead-code-merge oop-mark loss (#2) is an x86-64-backend-specific fix;
the ARM64 backend (`regalloc::allocate_registers_arm64` and its own codegen)
was not audited for an equivalent gap — no ARM64 hardware was available in
this session to reproduce/verify. If an ARM64-specific `Compiler` has its
own branch-target/dead-code-merge machinery mirroring x64.rs's, it may share
this defect.
