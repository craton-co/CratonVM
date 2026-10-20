# `CompletableFuture.complete` / `thenApply` cost 17x / 16x HotSpot

Status: OPEN, narrowed by reading in JIT round 21 wave 3 (lane indy) and wave 13 (lane cf). Wave 13 built
one opt-in switch, `CRATONVM_JIT_IR_SPLICE_VOLATILE_STORE` (unbuilt); the census has not been run. Wave 22
(lane cf) added `CRATONVM_JIT_IR_FRESH_VOLATILE_STORE_UNFENCED` (unbuilt, opt-in; last section).
Area: compiled JDK bytecode of `java/util/concurrent/CompletableFuture`: the optimizing tier's splice
admission (`vm/src/runtime/interpreter/jit_bridge.rs` `resolve_inline_site_from`), the reference
constant-`VarHandle` accesses (lane updaters' area since r21 wave 3), the stage builders' calls
Found by: JIT round 21 wave 1, lane indy (target (b) of the round-20 handoff perf table)

## Reproducer and numbers

`tools/probes/jit/r21/R21CfChain.java`, run lines in its header. Windows, fat LTO, ns/op, as
HotSpot 25 / dev / r21 wave 1 (orchestrator run). Facts are equal to HotSpot in both modes.

| row | HotSpot | dev | wave 1 |
|---|---|---|---|
| cf-new (`new CF()` + `isDone()`) | 0 | 74 | 64 |
| cf-complete (`new` + `complete(v)`) | 9 | 238 | 231 |
| cf-complete-twice | 14 | 410 | 369 |
| cf-completed-future (`completedFuture(v).getNow(0)`) | 0 | 205 | 353 |
| cf-thenapply-done | 4 | 339 | 547 |
| cf-thenapply-capture | 4 | 370 | 585 |
| cf-thenapply-pending | 37 | 1091 | 1869 |
| vh-cas-control (own-class ref CAS + a small `new`) | 8 | 11 | 20 |
| vh-get-control (own-class ref `getAcquire`) | 0 | 293 | 425 |
| lambda-apply-control | 0 | 23 | 40 |

## What the rows say

* **The allocation is not the cost.** A small `new` is about 3-6 ns here, and `vh-cas-control`
  (a `new` plus a reference CAS) is 11. So `cf-new` at 64 ns and `cf-complete` at 231 ns are calls
  or refused splices, not the TLAB.
* **The reference CAS is cheap; reference `get` / `set` through a VarHandle are not.**
  `vh-cas-control` is 11, `vh-get-control` is 293. `varhandle_ir_const_bind_entry` (jit/src/lib.rs)
  states it: "Reference returns keep their call". JDK 25's `CompletableFuture(Object r)` is
  `RESULT.setRelease(this, r)`, which is on the `completedFuture` / `thenApply` path (every
  `completedFuture` and every `uniApplyNow` result). That access belongs to lane updaters
  (the vh-get-control row); this page only records that it explains a part of `cf-completed-future`.
* **Wave 1 is slower than dev on four rows** (completed-future 205 -> 353, thenapply-done 339 -> 547,
  thenapply-capture 370 -> 585, pending 1091 -> 1869). The controls moved too (vh-get 293 -> 425,
  lambda-apply 23 -> 40), so it is probably not CF-specific. Lane indy's wave-1 default-ON switch is
  `CRATONVM_JIT_STATIC_INIT_RACE_SERVE`; it adds at most one compile per raced class, not a
  steady-state cost, but arm 3 below rules it out. Lane indy's other wave-1 switches are opt-in.

## Reading: what the splice planner will do with each JDK 25 method

The refusal names are those of `resolve_inline_site_from`'s `no!` macro (printed as `[cratonvm-jitc]
inline-resolve REFUSED <class>.<method><desc> depth=N: <reason>`). Each of these is a PREDICTION from
the bytecode and the admission code, to be confirmed by the census.

| method (JDK 25) | shape | predicted verdict |
|---|---|---|
| `<init>()V` | `Object.<init>` only | spliced (or an elided init) |
| `isDone()Z` | `result != null` | spliced; exact receiver after a `new` |
| `complete(T)Z` | calls `completeValue` and `postComplete`, one return | spliced at depth 0; its two calls nested |
| `completeValue(T)Z` | `RESULT.compareAndSet(this, null, t == null ? NIL : t)`: a branch, a `getstatic NIL`, a VarHandle CAS | admissible (`CRATONVM_JIT_IR_SPLICE_BRANCH`, `ir_splice_getstatic_enabled`, `CRATONVM_JIT_SPLICE_VH_IR_BOUND`), if the nearest planned `getstatic` before the CAS is `RESULT` |
| `postComplete()V` | a `while` loop over `stack` with `STACK.compareAndSet`, `NEXT.compareAndSet`, the abstract `Completion.tryFire`, `pushStack` | likely refused, so a compiled CALL per `complete`: an abstract/unprofiled virtual call that cannot survive in a splice, and the loop with three VarHandle CAS sites |
| `getNow(T)` | `ldc "getNow"` before `reportJoin` | refused, `ldc-non-numeric-constant` / `ir-splice-ldc`: a call per op on every `getNow` row |
| `completedFuture(U)` | branch, `getstatic NIL`, `new CF(Object)` -> `RESULT.setRelease` | the constructor's reference `setRelease` keeps its VarHandle call (see above) |
| `thenApply(F)` -> `uniApplyStage(E,F)` | `requireNonNull`, two `return`s (`return uniApplyNow(..)` / `return d`) | `ir-splice-not-single-trailing-return` unless `CRATONVM_JIT_IR_SPLICE_MULTI_RETURN` (chain-arm stage) is on |
| `uniApplyNow(Object,E,F)` | `instanceof AltResult`, a try/catch around `f.apply`, three returns, `newIncompleteFuture()` (a virtual call) | refused on its exception table (`callee-exception-table (...)`, SPLICE20-5) or on multiple returns: a compiled call |
| `newIncompleteFuture()` | `return new CompletableFuture<U>()`, virtual, overridden by `MinimalStage` | spliced only with an exact or monomorphic receiver (`ir-splice-target-not-provably-monomorphic` otherwise) |

The per-op budget reads as follows. `complete` = `completeValue` (spliced or not, ~11 when it is) +
one compiled call into `postComplete` (whose own body is a loop with three VarHandle sites) + the
`new`. The ~170 ns above `cf-new` should be that call and whatever `postComplete`'s compiled body
does before it sees `stack == null`. `thenApply` = `uniApplyStage` + `uniApplyNow` as two compiled
calls (refused splices), a virtual `newIncompleteFuture`, the `RESULT.setRelease` call of the
constructor, the interface `apply`, and `getNow`'s call.

## The commands that decide it (for the orchestrator)

`P` is the probe class directory; one run each, not for timing.

1. Refusal census, both modes:
   ```
   CRATONVM_DBG_JITC=1 cratonvm -Xmx512m -cp $P R21CfChain 2>&1 \
     | grep -E "inline-resolve REFUSED java/util/concurrent/CompletableFuture|\[ir\] blind-dispatch .*CompletableFuture" \
     | sed -E 's/ depth=[0-9]+//' | sort | uniq -c | sort -rn | head -40
   ```
   (repeat with `--compatible`). Reasons to read: `ir-splice-not-single-trailing-return`
   (uniApplyStage / uniApplyNow), `callee-exception-table (...)` (uniApplyNow), `ldc-non-numeric-constant`
   (getNow), and whatever `postComplete` prints. A `blind-dispatch` line naming a CF method is a by-name
   helper call per execution (~175 ns): that is the first thing to fix.
2. Which tier compiled the kernels and the CF methods:
   ```
   CRATONVM_DBG_JITC=1 cratonvm -Xmx512m -cp $P R21CfChain 2>&1 \
     | grep -E "(R21CfChain\.(cfNew|complete|completedFuture|thenApplyDone)|CompletableFuture\.(complete|completeValue|postComplete|uniApplyStage|uniApplyNow|getNow|<init>))" \
     | grep -E "full-compile|optimizing|single-pass|bailout|ir-bailout" | sort | uniq -c | head -60
   ```
3. Wave-1 regression arm, timing, 3 interleaved reps:
   `CRATONVM_JIT_STATIC_INIT_RACE_SERVE=0` vs default on cf-completed-future / cf-thenapply-done /
   vh-get-control. Equal rows clear lane indy's switch.
4. Multi-return splice arm, timing: `CRATONVM_JIT_IR_SPLICE_MULTI_RETURN=1` vs default on the
   cf-thenapply rows. A drop names `uniApplyStage`'s two returns as a cost and says what to flip or
   fix.

## What would close it

Each item depends on what the census confirms:
* A `postComplete` that stays a call but returns at once (`stack == null`).
* A spliceable `uniApplyStage` (the multi-return arm).
* `getNow`'s String `ldc` admitted in splices (lane zLdc's area).
* The reference VarHandle `get` / `setRelease` inline (lane updaters).
* Any `blind-dispatch` CF site bound directly.

None is in lane indy's area (call sites, indy linkage, class-init barriers), so this lane made no code
change for this row.

## JIT round 21 wave 13 (lane cf): reading, one opt-in switch, the census to run

Built, not run: no build, probe or census was run in this lane. Everything below is from reading
`resolve_inline_site_from` (`vm/src/runtime/interpreter/jit_bridge.rs`), the IR builder (`jit/src/ir.rs`)
and `append_ir_inline_site` (`jit/src/lib.rs`). The census below confirms or rules out each finding.

### What the reading found

1. **`uniApplyNow` was refused in every arm, including the wave-3 handler arms.** The body ends its
   `try` with `d.result = d.encodeValue(f.apply(t))`, and `CompletableFuture.result` is `volatile`. The
   resolver's field pass refused any body with a volatile `putfield`, in both tiers
   (`splice-volatile-field-store`). Its note said "the optimizing tier has no fenced spliced store",
   but that is no longer true:
   * The builder's `putfield` arm builds any pc in `volatile_field_pcs` as an `Op::Store` marked
     `volatile_access`. `add_spliced_volatile_field_pcs` sets the store admission bit for spliced pcs.
   * `ir_lower` follows every `is_volatile_store()` node with `LOCK ADD [RSP], 0`.

   The field pass runs after the exception-table check. So `CRATONVM_JIT_IR_SPLICE_CALLEE_HANDLERS_WIDE=1`
   (wave 3) only moves `uniApplyNow`'s refusal from `callee-exception-table (handlers: catches
   StackOverflowError)` to `splice-volatile-field-store`. Both thenApply rows pay for a compiled
   `uniApplyNow` call whichever arm is used. This is the new switch below. (The handler's own
   `d.result = encodeThrowable(ex)` is in the masked copy, so the scan never sees it.)
2. **`f.apply(t)` inside the stage stays an interface call.** A lambda proxy receiver is never
   spliced (`receiver-is-lambda-proxy`, because a proxy's dispatch is synthesised). This is
   `lambda-apply-control`'s cost, paid once per `thenApply`. It is outside the admission rules.
3. **`postComplete` is admissible by reading**, so it is not the predicted refusal:
   * branches and loops are admitted (`CRATONVM_JIT_IR_SPLICE_BRANCH`);
   * volatile `stack` / `next` loads are ordered loads;
   * the `STACK` / `NEXT` CASes are constant-VarHandle sites;
   * the abstract `tryFire` survives as a virtual call with its own MIC (the IR arm has no "every
     call direct-bound" rule);
   * the body has one trailing `return`.

   If the census still prints a `tier=ir` refusal for `postComplete`, that line names the fix. If it
   prints none, the ~170 ns between `cf-new` and `cf-complete` is not admission. Look at publication
   instead: wave 3 item 6 of `docs/internal/proposals/jit/jit-r21-splice-proposals.md` (`jit-cache-put
   DROPPED` / `c2-publication REFUSED ... CompletableFuture.complete`). In that case the kernel runs a
   single-pass body, and the single-pass inliner refuses `new`, `ldc "getNow"` and type checks.
4. **`getNow`'s `ldc "getNow"`** is admitted by the optimizing tier once the String is resolved
   (`CRATONVM_JIT_IR_SPLICE_REF_LDC`). On these rows the `ldc` runs on every op, so it is resolved.
   A `ldc-non-numeric-constant` line without `(callee pc ..: not resolved or no slot)` comes from the
   single-pass tier.
5. **`completedFuture`'s `RESULT.setRelease`** (the `CompletableFuture(Object)` constructor) is lane
   updaters' row (`r21-vh-const-refuses-cast-and-statement-sites-20261019.md`). Not changed here.

### Switch (opt-in)

| switch | default | reader | what it does |
|---|---|---|---|
| `CRATONVM_JIT_IR_SPLICE_VOLATILE_STORE` | off | `jit` `ir::ir_splice_volatile_store_enabled` (token `ir-splice-volatile-store`) | An optimizing-tier splice keeps a volatile `putfield` instead of refusing the body. The resolver records the pc in `InlineSite::volatile_field_pcs` beside the volatile loads. `append_ir_inline_site` rebases it (`ir::spliced_volatile_row_admitted`). The builder builds it as an `Op::Store` marked volatile, which `ir_lower` fences. |

Limits of the switch:
* The single-pass inliner (plain store, no fence) keeps the refusal.
* The switch is off whenever `CRATONVM_JIT_IR_VOLATILE_FIELDS=0`.
* A store into an object the splice did not allocate is a committed store
  (`CRATONVM_JIT_IR_SPLICE_COMMITTED_STORE`): the walk refuses any later trap in the splice, so no
  deopt re-executes the store.

Tests (`cargo test -p cratonvm-jit --lib r21w13_cf_`):
* `r21w13_cf_a_spliced_volatile_putfield_builds_a_volatile_store`: the builder side, with store
  admission on and off, plus a plain control.
* `r21w13_cf_spliced_volatile_rows_admit_a_putfield_only_under_the_switch`: the row rule and the
  reader's gating.

### Census recipe (orchestrator; one run each, not for timing)

`P` is the probe class directory (`javac -d tools/probes/out tools/probes/jit/r21/R21CfChain.java`).
Run every census in `--jdk-only` (the launcher default) and in `--compatible`.

1. **Optimizing-tier refusals.** Base arm, then the handler arms, then the handler arms plus this switch:
   ```
   H="CRATONVM_JIT_IR_SPLICE_CALLEE_HANDLERS=1 CRATONVM_JIT_IR_SPLICE_CALLEE_HANDLERS_WIDE=1 CRATONVM_JIT_IR_CALL_SITE_EXCEPTION_EDGES=1"
   for ARM in "" "$H" "$H CRATONVM_JIT_IR_SPLICE_VOLATILE_STORE=1"; do
     echo "== $ARM"
     env $ARM CRATONVM_DBG_JITC=1 cratonvm -Xmx512m -cp $P R21CfChain 2>&1 \
       | grep -E "inline-resolve REFUSED java/util/concurrent/CompletableFuture.*tier=ir|\[ir\] blind-dispatch .*CompletableFuture|jit-cache-put DROPPED|c2-publication REFUSED" \
       | sed -E 's/ depth=[0-9]+//; s/receiver=[0-9]+/receiver=N/' | sort | uniq -c | sort -rn | head -40
   done
   ```
   Expected by reading:
   * arm 1: `uniApplyNow ... callee-exception-table (handlers: catches StackOverflowError)`;
   * arm 2: `uniApplyNow ... splice-volatile-field-store`;
   * arm 3: no `uniApplyNow` line.

   Also read whatever `postComplete` / `uniApplyStage` / `getNow` / `reportJoin` print in each arm.
2. **Which tier runs each kernel, and why a tier-up was declined or bailed:**
   ```
   CRATONVM_DBG=jit-compiled,tierup-decline,ir-bailout cratonvm -Xmx512m -cp $P R21CfChain 2>&1 \
     | grep -E "R21CfChain\.|CompletableFuture\.|\[ir-bailout\]|tierup-decline" \
     | sort | uniq -c | sort -rn | head -60
   ```
   Then repeat with the arm-3 variables. Rows to read:
   * the kernels of `cf-complete`, `cf-thenapply-done` and `cf-thenapply-pending` must show an
     optimizing `put` line;
   * an `[ir-bailout]` at a spliced pc under arm 3 names a builder limit this switch exposed.

### A/B (timing; 3 interleaved reps, medians, P-cores pinned)

| arm | variables | rows to read |
|---|---|---|
| A (control) | wave-3 handler trio: `CRATONVM_JIT_IR_SPLICE_CALLEE_HANDLERS=1 CRATONVM_JIT_IR_SPLICE_CALLEE_HANDLERS_WIDE=1 CRATONVM_JIT_IR_CALL_SITE_EXCEPTION_EDGES=1` | cf-thenapply-done, cf-thenapply-capture, cf-thenapply-pending, cf-completed-future |
| B | A + `CRATONVM_JIT_IR_SPLICE_VOLATILE_STORE=1` | same rows |
| C | default + `CRATONVM_JIT_IR_SPLICE_VOLATILE_STORE=1` alone | every row: should equal default (the switch alone adds no splice here, because `uniApplyNow` still refuses on its handler) |

Gates:
* Every non-`time` line must equal HotSpot 25's in both modes on every arm.
* Expected move: B below A on the two `cf-thenapply-done/capture` rows, because `uniApplyNow`'s call
  disappears.
* A B worse than A means the spliced handler path (deopt into `uniApplyNow`'s handler) costs more than
  the call. Keep the switch off and record that.
* Run `R21SpliceRows` and `R21UpdaterRows` under C as a cross-check, because the switch applies to any
  callee with a volatile `putfield`. `R21UpdaterRows` declares volatile fields, so its helpers may now
  splice. Facts must stay equal to HotSpot's. A row that gets slower names a splice that loses to its
  call.

## JIT round 21 wave 16 (lane perf-cf-lean): one opt-in switch, one design

Built, not run: no build, probe or census was run in this lane. The wave-13 census above has still not
been run, so each item below is still a reading.

### Item status

| item | status | where |
|---|---|---|
| reference CAS from `null` / set over `null` decline to the VarHandle call while a mark runs | FIXED (candidate, pending orchestrator build and A/B) | `jit/src/ir_lower.rs` `vh_const_satb_null_old_enabled`, `vh_const_satb_null_old`, `vh_ref_store_gate_screen_with`, called from `vh_const_core_bytes` (both tiers: `x64/op_invoke.rs` uses the same bytes) |
| `postComplete` call per `complete` | FIXED (candidate, pending orchestrator build and A/B), `CRATONVM_JIT_IR_CALL_ENTRY_NULL_GUARD` | `jit/src/ir_lower.rs` `try_emit_ir_call_entry_null_guard_prefix` (asked last in the `Op::Call` prefix chain), `call_entry_null_guard_bytes`, `call_entry_null_guard_ask`, `WELL_KNOWN_CALL_ENTRY_NULL_GUARD_TAG`; `vm/src/runtime/interpreter/jit_bridge.rs` `entry_null_guard_field`, `call_entry_null_guard_answer`; `vm/src/jit/helpers.rs` `jit_well_known_class_id` (the tag's arm) |
| `uniApplyStage` / `uniApplyNow` / `getNow` splices | unchanged (wave 3 / wave 13 switches; the census decides) | |
| `RESULT.setRelease` in `CompletableFuture(Object)` | lane updaters' row; served by the existing prefix when its gates are published. The new switch keeps it inline while marking too | |

### `CRATONVM_JIT_VH_CONST_SATB_NULL_OLD` (opt-in)

The constant-`VarHandle` prefix's barrier screen sent EVERY reference store to the call while the SATB
pre-gate byte is set (ZGC `mark_active`, G1's queue gate). A call costs ~300 ns here (the
`vh-get-control` row). The pre-barrier only logs the overwritten value, and both collectors' barrier
returns at once on a null old value (`gc/src/zgc.rs` `satb_pre_barrier_slow`, `gc/src/g1.rs`
`satb_pre_barrier`). HotSpot's G1 pre-barrier filters it the same way. With the switch, while marking:

* a CAS / compareAndExchange whose `expected` is null stays inline. It overwrites exactly `expected`
  or nothing, so there is no race. This covers `completeValue`'s `RESULT.compareAndSet(this, null, v)`
  and `tryPushStack`'s first push;
* a `set*` reads the field first and stays inline when it is null. This covers the
  `CompletableFuture(Object)` constructor's `RESULT.setRelease` and `NEXT.set(c, h)` on a fresh
  completion. The read-to-store window is HotSpot's own and harmless for the same reason (see the
  doc on `vh_const_satb_null_old_enabled`);
* getAndSet / getAndBitwise keep the decline, and so does a non-null old value or `expected`;
* the post gates (young receiver, no old objects) run unchanged.

Off, the bytes are exactly the previous ones.

Test: `cargo test -p cratonvm-jit --lib r21w16_cf_`
(`r21w16_cf_a_null_old_value_store_stays_inline_while_marking_only_under_the_switch`, which executes
the bytes against a fake object and gate bytes, on and off).

What to expect: `vh-cas-control` (the same CAS shape) is 11 ns at `-Xmx512m`, so marking is rare in that
run and the default-heap rows may not move. The arm that shows the switch is a heap small enough to
keep a mark running. The switch helps only rows whose reference store reaches the prefix at all: run
the engagement grep first.

### Commands (orchestrator)

`P=tools/probes/out` (`javac -d tools/probes/out tools/probes/jit/r21/R21CfChain.java tools/probes/jit/r21/R21UpdaterRows.java`).

1. Engagement (one run, not for timing). Which CF reference stores take the prefix, and with gates:
   ```
   CRATONVM_DBG_JITC=1 cratonvm -Xmx512m -cp $P R21CfChain 2>&1 \
     | grep -E "vh-const-inline \((ir|sp)\).*kind=L" | sed -E 's/ n[0-9]+ / /' | sort | uniq -c | sort -rn | head
   ```
   `gated=true` lines for `compareAndSet` / `setRelease` are the sites the switch can change.
2. How often a mark runs: `CRATONVM_GC_STATS=1 cratonvm --verbose:gc -Xmx64m -cp $P R21CfChain 2>&1 | grep -ciE "concurrent mark|mark start"`.
3. A/B (timing; 3 interleaved reps, medians, P-cores pinned), both modes:
   ```
   for HEAP in -Xmx512m -Xmx64m; do
     for ARM in "" "CRATONVM_JIT_VH_CONST_SATB_NULL_OLD=1"; do
       echo "== $HEAP $ARM"; env $ARM cratonvm $HEAP -cp $P R21CfChain
     done
   done
   ```
   Repeat with `-XX:+UseG1GC`. Rows: `cf-complete`, `cf-completed-future`, `cf-thenapply-*`,
   `vh-cas-control`. Expected: equal at `-Xmx512m`, lower at `-Xmx64m` if (1) shows gated sites.
4. Cross-check: `R21UpdaterRows` under the switch at `-Xmx64m`. Every fact must equal HotSpot 25.

Gates: every non-`time` line equals HotSpot's in both modes on every arm. Keep the switch off if no row
moves.

### Design (superseded by the build below): a `postComplete` that stays a call but skips it when `stack == null`

This needs `jit_bridge.rs` (resolver) and `ir.rs` (builder), which other lanes own this wave.

* **Resolver** (`resolve_inline_site_from`, `ir_mode`). Take a callee whose splice is refused, or
  which the planner keeps as a call (size, loop), when all of these hold:
  * it is `void`, exactly bound (final / private / exact receiver), with no exception table;
  * its body begins with a pure entry guard: `aload_0; [astore_1; aload_1;] getfield F;
    [dup; astore_2;] ifnonnull BODY`. The fall-through reaches `return` through compares of locals
    only, with no store, call or allocation.

  For `postComplete` the fall-through is `aload_1; aload_0; if_acmpeq EXIT` with local 1 == this, so
  it always exits. The resolver records an `EntryNullGuard { field, volatile }` beside the call site
  instead of an `InlineSite`.
* **Builder.** At that call:
  * emit `LoadField(recv, F)`, an ordered load when `F` is volatile (`CompletableFuture.stack` is), as
    the IR already builds volatile loads;
  * then `If(IsNull)` → join, and else the unchanged `Op::Call`;
  * the receiver's null check stays the call's own: guard only after a proven non-null receiver,
    else keep the call.
* **Why it is sound.** The skipped path is exactly what the callee would execute: a read of `F`,
  then the return. A racing push between the guard and the return is the same race the callee has
  between its read and its return.
* **Switch:** `CRATONVM_JIT_IR_CALL_ENTRY_NULL_GUARD`, opt-in. Rows: `cf-complete` and
  `cf-complete-twice` should lose the `postComplete` call (the ~170 ns between `cf-new` and
  `cf-complete`, if census item 2 above shows `postComplete` as a compiled call).
* **Do not build it** if the census shows `postComplete` spliced. Then the cost is elsewhere, and
  census command 2 of wave 13 names it.

### Built (wave 16, after the self-copy lane released `jit_bridge.rs` / `ir.rs`): `CRATONVM_JIT_IR_CALL_ENTRY_NULL_GUARD` (opt-in)

Unbuilt and unrun, like the rest of this wave. This is not the builder form designed above. It is a
lowering prefix of the `Op::Call`, the same contract as the other call prefixes. The builder
(`jit/src/ir.rs`) is unchanged. The prefix also covers a `postComplete` call inside a spliced
`complete`, because that is still an `Op::Call` node.

* **Compile time (VM, `jit_bridge::call_entry_null_guard_answer`).** The lowering asks through
  `jit_well_known_class_id` with a new tag (`0x5651 << 48`, a `CallEntryNullGuardQuery` address). The VM
  answers only when all of these hold:
  * the target is receiver-bound: private, final, a final declaring class, or a final constant-pool
    class. An `invokespecial` must name a private method of the constant-pool class itself;
  * it is a `void` instance method, not native, abstract or synchronized, and not an initializer or
    an interface method;
  * it is the method JVMS resolution answers;
  * no registered native shadows it, and no breakpoint is set in it;
  * `entry_null_guard_field` finds its entry shape. The entry is executed abstractly with
    `this.F == null`: loads and stores of locals, `dup` / `pop` / `aconst_null`, `getfield` of `this`
    (one pool index only), and branches whose answer is known, ending in `return`;
  * `F` resolves loader-faithfully to a reference instance field with a compact displacement.

  On an answer the target class is recorded as copied bytecode (`note_bytecode_copied`), so a
  redefinition of it withdraws the body.
* **Run time (`call_entry_null_guard_bytes`).** After the layout-epoch guard, the call is skipped
  only for a receiver that is non-null, a plain object, compact, and whose `[RAX + disp]` is 0. Every
  other receiver goes to the unchanged call, which also raises the null receiver's NPE. There is no
  call, GC point or deopt on the skip path.
* **Refused at compile time:** narrow oops, an armed ZGC read barrier, a static or interface site, a
  non-`void` descriptor, and a carried receiver.
* **Not handled:** a JVMTI MethodEntry event for the skipped callee. That is the same gap a splice has.

Tests:
* `cargo test -p cratonvm-jit --lib r21w16_cf_`:
  * `r21w16_cf_the_answer_word_round_trips_and_rejects_foreign_words`;
  * `r21w16_cf_the_switch_is_default_off`;
  * `r21w16_cf_the_guard_skips_only_a_null_field_of_a_compact_receiver` (the bytes, executed);
  * plus the SATB test above.
* `cargo test -p cratonvm-vm --lib r21w16_cf_` (the VM matcher):
  * `r21w16_cf_post_complete_s_entry_is_a_null_guard_on_its_field`;
  * `r21w16_cf_any_effect_or_unknown_on_the_null_path_refuses`.

Commands (orchestrator; `P` as above):
1. Engagement, in both modes:
   ```
   CRATONVM_JIT_IR_CALL_ENTRY_NULL_GUARD=1 CRATONVM_DBG_JITC=1 cratonvm -Xmx512m -cp $P R21CfChain 2>&1 \
     | grep -E "call-entry-null-guard (ADMITTED|\(ir\))" | sed -E 's/ n[0-9]+ / /' | sort | uniq -c | sort -rn | head
   ```
   Expect `ADMITTED java/util/concurrent/CompletableFuture.postComplete()V ... volatile=true`, and one
   `(ir)` line per compiled `postComplete` call site. If there is no `(ir)` line, `postComplete` is
   spliced (or `complete` is not compiled by the optimizing tier), and this switch cannot move the
   rows.
2. A/B (timing; 3 interleaved reps, medians, P-cores pinned), both modes:
   ```
   for ARM in "" "CRATONVM_JIT_IR_CALL_ENTRY_NULL_GUARD=1"; do
     echo "== $ARM"; env $ARM cratonvm -Xmx512m -cp $P R21CfChain
   done
   ```
   Rows: expected to move are `cf-complete` and `cf-complete-twice` (the second `complete`'s CAS
   fails, then `postComplete` runs with an empty stack too). `cf-thenapply-pending` must not change:
   its `complete` sees a pushed completion, so it declines to the call. `cf-new` and the controls
   must be equal.
3. Cross-check: `R21SpliceRows` and `R21UpdaterRows` under the switch; every fact must equal HotSpot 25.
   Read the engagement grep's `ADMITTED` lines for anything outside `CompletableFuture`.

Gates: every non-`time` line equals HotSpot's in both modes on every arm. Keep the switch off if
`cf-complete` does not move.

## JIT round 21 wave 22 (lane cf): `CRATONVM_JIT_IR_FRESH_VOLATILE_STORE_UNFENCED` (opt-in)

Unbuilt and unrun: no build, test, probe or census was run in this lane.

### Which refusal, and why this one

The refusals still open on this page are each owned by a switch already built (multi-return,
callee handlers, volatile splice store, ref `ldc`, call-entry null guard) or by another lane
(`RESULT.setRelease` / `NEXT.set`: lane vhconst). CHA devirtualization of `newIncompleteFuture` or
`Completion.tryFire` is ruled out by the resolver's own contract (`resolve_inline_site_from`: "Deliberately
NOT a CHA-style answer ... needs an invalidation dependency the IR path does not record"), and `tryFire`
has many loaded implementations anyway. What is left inside the JIT is the cost wave 13's switch
CREATES: once `uniApplyNow` splices (`CRATONVM_JIT_IR_SPLICE_VOLATILE_STORE`), its
`d.result = d.encodeValue(f.apply(t))` is a volatile `putfield` that `ir_lower` follows with
`LOCK ADD [RSP], 0`, although `d` is the `newIncompleteFuture()` allocation of the same splice and no
other thread can hold it yet. The same fence follows every volatile field write in a spliced
constructor (`new AtomicInteger(v)`, `ConcurrentHashMap.Node`'s `val` / `next`, ...).

### What changed

`jit/src/ir_lower.rs` only:
* `fresh_allocation_unfenced_volatile_stores` (with `fence_escape_reaches`, `FenceBlockView`) computes, from
  the finished schedule, the volatile `Op::Store`s whose base is directly an `Op::New` that cannot have
  escaped on any path from the allocation to the store. Escape = any use of the allocation other than the
  base input of an `Op::Load` / `Op::Store` (call argument, stored value, phi, return, compare, cast,
  monitor, ...). A use the schedule does not place refuses every store into that allocation. The search
  stops at the allocation's own block (entering it allocates a new object), so a loop that returns to the
  store without reallocating keeps the fence. The doc comment on the function holds the JMM/TSO argument:
  StoreLoad can only be observed by a thread that reads the field, which needs the object, whose
  publication comes later in program order and drains after this store.
* `Lowerer::volatile_store_fence_elided` computes the set once per compile (at its first volatile store)
  and the `lower_block` fence site skips the `LOCK ADD` for a member. Off, the set is empty and the bytes
  are the previous ones.
* Engagement line (`CRATONVM_DBG_JITC=1`): `[cratonvm-jitc] ir-fresh-volatile-store-unfenced <method> pc=N nID`.
* Single-pass tier, `putstatic` and VarHandle sites are unchanged.
* Known limit, the same one HotSpot's escape analysis opts out of: a JVMTI agent reading a local at a
  safepoint between the allocation and the store could publish the object itself.

| switch | default | reader | what it does |
|---|---|---|---|
| `CRATONVM_JIT_IR_FRESH_VOLATILE_STORE_UNFENCED` | off | `jit` `ir_lower::ir_fresh_volatile_store_unfenced_enabled` | The optimizing tier skips the StoreLoad fence after a volatile `putfield` into an object the compile allocated, when no escaping use can run between the allocation and the store. |

Tests (`cargo test -p cratonvm-jit --lib r21w22_cf_`), module
`r21w22_cf_fresh_volatile_store_fence_tests`:
* `r21w22_cf_the_switch_is_default_off`;
* `r21w22_cf_a_store_before_the_first_escape_is_unfenced` (a field read through the object and an
  unrelated call before the store are not escapes);
* `r21w22_cf_an_escape_before_the_store_keeps_the_fence` (a call argument, a store as value, a self store;
  a plain store and a store into a parameter are never candidates);
* `r21w22_cf_an_escape_on_any_path_to_the_store_keeps_the_fence` (a diamond with an escape on one arm and
  a phi at the join);
* `r21w22_cf_a_back_edge_reaches_the_store_only_without_a_new_allocation`;
* `r21w22_cf_an_unscheduled_escape_refuses`.

### Expected effect

Small and only where the store is compiled at all: one `LOCK ADD` (~5-20 ns on these cores, more with a
full store buffer) per `thenApply` on a completed future, IF `uniApplyNow` and `newIncompleteFuture` are
spliced into the kernel (wave-13 arm B: the handler trio plus `CRATONVM_JIT_IR_SPLICE_VOLATILE_STORE`). Under
the default arms `uniApplyNow` is a compiled call whose own compile may still engage (its `d` is spliced
from `newIncompleteFuture` there too). Rows: `cf-thenapply-done`, `cf-thenapply-capture`. No other row
should move.

### Commands (orchestrator; `P=tools/probes/out`, `javac -d $P tools/probes/jit/r21/R21CfChain.java tools/probes/jit/r21/R21UpdaterRows.java`)

1. Engagement, one run each, both modes (`--jdk-only` default and `--compatible`):
   ```
   B="CRATONVM_JIT_IR_SPLICE_CALLEE_HANDLERS=1 CRATONVM_JIT_IR_SPLICE_CALLEE_HANDLERS_WIDE=1 CRATONVM_JIT_IR_CALL_SITE_EXCEPTION_EDGES=1 CRATONVM_JIT_IR_SPLICE_VOLATILE_STORE=1"
   for ARM in "CRATONVM_JIT_IR_FRESH_VOLATILE_STORE_UNFENCED=1" "$B CRATONVM_JIT_IR_FRESH_VOLATILE_STORE_UNFENCED=1"; do
     echo "== $ARM"
     env $ARM CRATONVM_DBG_JITC=1 cratonvm -Xmx512m -cp $P R21CfChain 2>&1 \
       | grep -E "ir-fresh-volatile-store-unfenced" | sed -E 's/ n[0-9]+$//' | sort | uniq -c | sort -rn | head -20
   done
   ```
   Expect lines naming `CompletableFuture.uniApplyNow` (own compile) and/or the `R21CfChain` thenApply
   kernels (spliced). No line means no volatile store into a fresh object is compiled on these rows, and
   the switch cannot move them.
2. A/B, timing, 3 interleaved reps, medians, P-cores pinned (`start /affinity FFFF`), both modes:
   ```
   for REP in 1 2 3; do
     for ARM in "$B" "$B CRATONVM_JIT_IR_FRESH_VOLATILE_STORE_UNFENCED=1" "" "CRATONVM_JIT_IR_FRESH_VOLATILE_STORE_UNFENCED=1"; do
       echo "== rep $REP: $ARM"; env $ARM cratonvm -Xmx512m -cp $P R21CfChain | grep -E "^time"
     done
   done
   ```
   Rows: `cf-thenapply-done`, `cf-thenapply-capture` (expected lower with the switch, by a few ns); every
   other row equal.
3. Cross-check under the switch alone and under `$B` plus the switch: `R21CfChain`, `R21UpdaterRows`,
   `R21SpliceRows` non-`time` lines equal HotSpot 25 (`java -Xmx512m -cp $P <class>`) in both modes.

Gates: every non-`time` line equals HotSpot's in both modes on every arm. Keep the switch off if no
thenApply row moves beyond noise.
