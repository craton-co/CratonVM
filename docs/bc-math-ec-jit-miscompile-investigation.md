# bc math-ec JIT miscompile — investigation notes

Status: **PARTIAL**. Two genuine, sound codegen correctness fixes landed in
`jit/src/x64.rs`. The *specific* `org.bouncycastle.math.ec.test.AllTests`
SEGV is **not yet fixed**; the blanket `org/bouncycastle/` JIT ban in
`vm/src/jit/skip_list.rs` is therefore **kept in place** (not removed).

> ## ⚡ BREAKTHROUGH 2026-06-05 (session "continue investigating") — a working
> **software watchpoint** proves the `0x4` is **written DURING `collect_garbage`**
> (the GC IS the corruptor). This **overturns** the "MAJOR REFRAME"/"mutator raw
> write" conclusion immediately below — that earlier session's `BADREF`-silent +
> `forward_object`-can't-return-`<0x1000` reasoning was right about those
> *specific* paths but wrong to conclude "mutator": the GC produces `0x4`
> through a DIFFERENT collector path.
>
> ### The tool (left in tree, gated default-OFF)
> `CRATONVM_DBG_ECWATCH` (`vm/src/runtime/ec_watch.rs`): when a *valid* reference
> is stored into a watched EC-holder field (interpreter `Putfield`; classes whose
> name contains `/asn1/x9/` or `Curve`), record `(ObjectRef, field_idx,
> expected)`. **Critically it REMAPS the watched holders through the GC
> `pointer_map` on every collection (`ec_watch::remap`) instead of clearing** —
> the first cut cleared on GC and caught NOTHING because the corruption hits
> objects that SURVIVE the GC that first wrote their field. Detect points:
> per-native (`CRATONVM_DBG_ECWATCH_NATIVE`, in `safe_native_call`), at GC ENTRY,
> and at **GC EXIT** (right after `collect_garbage`+`remap` in `maybe_gc`).
>
> ### What it showed (measured)
> - **GC-EXIT fired**: two watched cells clean at GC entry read `0x4` immediately
>   after `collect_garbage` — `holder@…748 fld[4]: 0x1aa25130 -> 0x4` and
>   `holder@…810 fld[2]: 0x2c0e31e0 -> 0x4` "corrupted DURING collect_garbage".
>   ⇒ **the minor (moving Cheney) GC writes the `0x4`.**
> - The corrupted `fld[4]` held an **OLD-gen** ref (`0x1aa25130`). The Cheney
>   ref-update only writes a field when `young_from.contains(field_value)` — an
>   old-gen ref is SKIPPED — yet it became `0x4`. So `0x4` is NOT from the
>   ref-update / `forward_object` (which also can't return an unaligned `4`); it
>   is a **wrong-target / wrong-size write** — most likely the object **copy**
>   using a too-small `total_size` (leaving the tail field = stale to-space
>   bytes) or a **scan-cursor desync** writing into a neighbouring object.
> - **Victim is random/long-lived** (`X9ECParameters`, but across runs also
>   `java/util/HexFormat`, `java/util/logging/Level`) — whatever object occupies
>   the target address. Confirms a fixed-/stray-address write, not EC-specific.
> - The per-native detect blames `BigInteger.multiply` / a native from
>   `LongArray.<init>` — but that is the native whose **allocation TRIGGERS the
>   GC**, not the writer; the GC it triggers does the damage (stack at detection
>   is always deep in EC field-multiply).
>
> ### Sound fix that landed (real bug, NOT the `0x4`)
> `native-builtins/src/lib.rs::bi_alloc_int` had a **native use-after-move**: it
> allocated the `BigInteger` `obj`, then `new_array(mag)` (which can GC and
> relocate the not-yet-rooted `obj`), then `set_field(obj,…)` through the stale
> ref. Fixed with `ctx.pin_native_root`/`read_native_pin` (the existing remap-
> aware native-pin API). This removes one stale-ref source (the `set_field`
> out-of-bounds flood) but does **not** stop the `0x4` (it writes `Int(signum)`/
> `Object(mag)` to fld[0]/[1], never `0x4` in fld[3]).
>
> ### `CRATONVM_DBG_GCWRITE` results (built + run) — narrows it hard
> Two instruments landed (gated `CRATONVM_DBG_GCWRITE`, gen_heap.rs): a per-copy
> source-vs-dest field compare after `copy_nonoverlapping`, and a
> `forward_object` return-value wrapper. Measured over many runs:
> - **`forward_object` NEVER returns a small (`<0x1000`) address** (`[gcwrite]
>   forward_object RETURNED` fired 0×). So NONE of the minor-GC ref-update writes
>   (which all write `forward_object`'s result) produce the `0x4`.
> - **The copy never CREATES the `0x4`**: every `[gcwrite] COPY fld[..]=0x4` has
>   **`src_small=true`** — the from-space SOURCE already held the `0x4`; the copy
>   only PROPAGATES it (verbatim, as expected). (Also the watch `detect()` is now
>   discriminant-checked — `tag==Object` AND payload `<0x1000` — so a field
>   re-assigned to `Int(4)`/`Long(4)` is not a false positive.)
> - **GC-EXIT STILL fires** (`holder fld[3]: 0x2541da70 -> 0x4 corrupted DURING
>   collect_garbage`), and the corrupted `fld[3]` held a young from-space ref the
>   Cheney scan WOULD forward — yet it became `0x4`, not the forwarded address.
>
> Net: the per-collect **SEED** of a new `0x4` is **neither `forward_object`'s
> value nor the copy** — it is a **wrong-target / literal write** somewhere in
> `collect_garbage` (the `src_small=true` cases are then just that seed
> propagating across cycles: this GC's to-space becomes next GC's from-space).
>
> ### Recommended next step (do FIRST next session)
> Two unexamined collector writes remain — instrument BOTH:
> 1. **`old_gen.rs` `update_references` (major-GC compaction)** writes
>    `Object(Some(forwarding_ptr))` read from the *referent's header* (NOT
>    `forward_object`). A stale/corrupt `forwarding_ptr == 4` (non-null, passes
>    the `!is_null()` guard) writes `Object(0x4)`. The seed correlates with
>    heap-pressure (clean at -Xmx1g/6g) → major GC is a prime suspect. Gate-log
>    `forwarding_ptr < 0x1000` there.
> 2. **Harden the watchpoint vs PROMOTION/major-GC remap.** `ec_watch::remap` is
>    single-step over `result.pointer_map`; under promote→compact the holder may
>    chain `H→H'→H''` or be reclaimed+reused. Before trusting a GC-EXIT hit,
>    re-validate the remapped holder's header `class_id` matches the watched
>    class — rules out a remap-false-positive so the GC-EXIT signal is airtight.
> The watchpoint + GC-EXIT are the reusable tools to confirm any fix → 0.

> ## ✅ MAJOR REFRAME 2026-06-05 (session "investigate this") — it's **Fp/SecP192R1**,
> NOT F2m; the GC is ~~provably not the writer~~ **[SUPERSEDED: the GC IS the
> writer — see BREAKTHROUGH above]**; `0x4` is a ~~mutator raw write~~.
> Repro hardened (`FixedPointTest`, `CRATONVM_DISABLE_JIT=1 -Xmx256m`, unique
> `cratonvm_ecjit.exe`) and now reproduces **every run** (~50 s). Fresh
> instrumented builds settled, decisively, several things the older blocks below
> got wrong. **Three new gated diagnostics are left in the tree** (default-OFF):
> `CRATONVM_DBG_RSET_AUDIT` (gen_heap.rs pre-GC remembered-set + `[small4]`
> small-ref scan of young+old), and `CRATONVM_DBG_HEAPCOPY`
> (vm_exec.rs `copy_to_native_memory` heap-dst tripwire + Java stack).
>
> ### What this session PROVED (each measured, not theorised)
> 1. **It is the prime field Fp, not F2m `LongArray`.** Every corrupt object is
>    an Fp/asn1.x9 type: `X9ECParameters` (`curve` f1, `n` f3), `X9Curve`
>    (`seed`/`curve` f1), `ECCurve$Fp` (`multiplier` f7), `ECPoint$Fp` (f4),
>    `ECFieldElement$Fp`. **SecP192R1** dominates. The F2m framing of every
>    block below is a dead end for this test. (`testFixedPointMultiplier` is
>    `referenceMultiply` vs `FixedPointCombMultiplier` over **all named curves**.)
> 2. **The failure is a WRONG RESULT / NPE, not (primarily) a crash.** Runs end
>    `expected:<…> but was:<…>` (a well-formed but wrong EC point) or
>    `NullPointerException: Cannot invoke isZero on null` in `ECPoint$Fp.twice`.
>    A corrupted precomp-table / coordinate object → wrong point.
> 3. **The remembered set is CORRECT.** A pre-GC audit (`CRATONVM_DBG_RSET_AUDIT`)
>    walks every old-gen object, checks each old→young edge against the card
>    bitmap: **`MISSES=0` on every GC**. So this is **NOT** an old→young
>    write-barrier / dirty-card miss (the `X9ECParametersHolder` history is a
>    red herring here).
> 4. **The GC CANNOT create `0x4`.** Exhaustive read of every GC field-write
>    site: they all write `Value::Object(Some(ObjectRef::from_raw(forward_object(…))))`
>    or `forward_object(…) as u64`. `forward_object` only ever returns `old_ptr`
>    (≥0x1000), a forwarding addr it gates on `%8==0 && !null`, or a fresh copy
>    addr — **never an unaligned `4`**. `0x4` is unaligned (`4 % 8 == 4`), so no
>    decode path (`to_value`/`decode_value`, both alignment-reject) and no GC
>    write can produce it.
> 5. **Therefore `0x4` is MUTATOR-written**, and `[small4]` confirms it:
>    `PRE-GC YOUNG X9ECParameters/X9Curve/ECCurve$Fp … fld[i] -> 0x4` fires on
>    **freshly-allocated young objects** — i.e. written between GCs to a *live*
>    object's reference field. Not a stale-after-relocation artifact.
> 6. **It bypasses the heap set API.** `CRATONVM_DBG_BADREF` (the `set_field` /
>    `set_array_element` `Object(Some(p<0x10000))` tripwire) stayed **silent in a
>    run that surfaced 8× `0x4`**. `putfield`/`aastore` both funnel through
>    `set_field`/`set_array_element` (`coerce_value_for_return` can only yield
>    `Object(None)` or a *huge* ptr from a `Long`, never `0x4`). So `0x4` is a
>    **raw cell write** that skips the set API.
> 7. **`copy_to_native_memory` is EXONERATED.** Its real impl (vm_exec.rs) raw-
>    copies to any `addr>0` not in an Unsafe arena — a genuine hazard — but the
>    `CRATONVM_DBG_HEAPCOPY` heap-dst tripwire fired **0×**. The only two callers
>    (`Unsafe.putByte` null-base, consolidated `copyMemory`) are not the path.
>
> ### Mechanism (high confidence) and what's still OPEN
> The heap is below 4 GiB (addrs like `0x1a9e_xxxx`, `0x2495_xxxx`,
> `0x2a95_xxxx` — high-32 == 0). So a **4-byte write of value `4`** (an int
> limb / small coefficient) to a reference field's **payload-low** half yields
> `payload = 0x0000_0000_0000_0004` with the disc still `Object` → exactly the
> observed `Value::Object(Some(0x4))`. The **diverse victim set** (different
> classes/fields each run) points at a write whose **target address varies** —
> most consistent with a **primitive int-array store overflowing its real
> allocation** into an adjacent object (an `Int` store does NOT trip `BADREF`,
> and `set_array_element` bounds-checks only against `header.array_length`, so an
> **undersized array** — real bytes < `array_length*4` — would write OOB while
> passing the check). **Not yet found:** the exact raw writer. Checked & cleared:
> all bytecode stores, all GC writes, `Unsafe.put{Int,Long,Object}`,
> `copy_to_native_memory`, the bulk byte/char array-region intrinsics, and the
> `BigInteger` `impl*ToLen`/`mulAdd` intrinsics (all bounds-checked).
>
> ### Recommended next step (do this FIRST next session)
> A **software watchpoint** is the right tool for "who writes `4` to address A":
> in `set_field`, when a *valid* ref is stored into a known-corruptible
> `(class, field)` (e.g. `X9ECParameters.curve`), record `(payload_addr,
> expected)`; **after every invoke-return** in the interpreter, re-read each
> watched payload and, the instant it flips to `4`, dump the full Java stack —
> the last-returned method is the corruptor (clear the watch-list on each GC to
> avoid stale addrs). The corruptor is a **native** (raw write), so post-invoke
> checking pins the native call. A hardware watchpoint (debug reg + the existing
> `crash_handler.rs` VEH) is the heavier alternative. Then look hard at array
> **allocation sizing** (`alloc_array`) for an undersized-`int[]` for some length.
> NB build trap this session: launching multiple background `cargo` builds leaves
> **orphaned cargo** that contend on `target/` and make every later build die at
> the cli link with a silent exit 1 — kill all `cargo`/`rustc` to 0 and build
> **foreground/undisturbed**.

> ## ⚠️⚠️ CORRECTION 2026-06-05 (session "fix that bug II") — the moving-collector
> framing is a **VERIFIER ARTIFACT**; the bug is **execution-time**, JIT-OFF, in
> the **moving Cheney + interpreter** path. Repro: `CRATONVM_DISABLE_JIT=1
> CRATONVM_DBG_HEAP_STALE=1 -Xmx256m … FixedPointTest` (uniquely-named
> `cratonvm_ecjit.exe`; ~40–55 s; corrupts in ~half of runs — highly
> nondeterministic). With JIT off, `gc_quiescence::is_active()` is false, so the
> **moving Cheney** `collect_garbage_inner` runs (NOT the non-moving sweep). This
> session **exhaustively ruled out** a long list of candidates and then a fresh
> read found that the central reframing tool was lying.
>
> ### What was DEFINITIVELY ruled out (each instrumented, gated, measured)
> The corrupt field is `Value::Object(Some(0x4))` — discriminant=Object (offset 0
> low-32 = 4), pointer payload at **offset+8 = exactly `0x0000000000000004`**
> (8 bytes; high-32 zero ⇒ NOT the raw collision long `0xFFFD000000000004`, but
> its 47-bit `& PAYLOAD_MASK` → 4). Heap fields are raw 16-byte `Value`
> (`read_slot`/`write_slot` = `ptr::read/write::<Value>`), no decode. Padding in
> offset 0 high-32 is the *old* referent address (register-leftover from the last
> full `Value::Object` write), so the corrupt cell is a **partial offset+8 write
> of 4** over a previously-valid `Object(Some(oldPtr))`.
>   - **`forward_object` NEVER returns `< 0x1000`** — wrapped the whole fn,
>     `POISON-FWD-RET` fired 0× across runs with 11–40 corrupt fields. So no
>     Cheney/dirty-card/promoted ref-update writes 4 (they all write
>     `Object(Some(forward_object(...)))`).
>   - **`set_field`/`set_array_element` never write `Object(Some(p<0x10000))`** —
>     pre-existing `CRATONVM_DBG_BADREF`, silent while 0x4 surfaces (verified with
>     BADREF *on* in the same run, not cross-run). All field-write APIs funnel
>     here (`set_field_volatile`/`_as` → `set_field`).
>   - **`write_prim_element` (Int 4-byte AND Long 8-byte) never clobbers a ref
>     payload** — instrumented to detect a small store landing on offset+8 of an
>     Object cell (disc==4 at tgt-8, old = 8-aligned heap ptr, high-32 zero); only
>     coincidental false positives on legit `long[]`/`int[]` elements
>     (`old=0/5`, `new=0/5`), never a real heap-ref being overwritten.
>   - **Operand stack AND locals do not mis-root collision longs** — `MISROOT` /
>     `MISROOT-LOCAL` (root pushed where `is_object_address` fails) fired 0×.
>     `local_kinds`/stack kinds are correct (`Lstore`→`set_local(Value::Long)`→
>     `LKIND_LONG`; all `set_local*`/`set_local_compact*` set the kind).
>   - **No two MOVED objects overlap** — checked `pointer_map.values()` ranges for
>     intersection, 0 overlaps.
>
> ### The reframing trap (THE key correction — do not repeat it)
> The `CRATONVM_DBG_HEAP_STALE` verifier (and the PRE/POST pre-GC variant added
> this session) walks the heap via `GenerationalHeap::walk_objects`
> (`gen_heap.rs:4386`). That young-gen walk is a **naïve linear stride that
> `break`s at the first header with implausible size** (`offset + total_size >
> used`, or `array_data_size` error — `gen_heap.rs:4432`). young-**from**
> (pre-GC) is fragmented and, in this workload, contains at least one
> dead/desynced object whose size word reads as a collision long ⇒ **the walk
> truncates and never reaches the live `X9ECParameters` beyond it**. So:
>   - **"PRE verify clean ⇒ this GC created the 0x4" is FALSE** — PRE is *vacuous*
>     (walk truncated). The moving GC compacts survivors **contiguously** into
>     to-space (no desync), so the POST walk reaches the object and *surfaces* a
>     0x4 that **pre-existed** (was carried across verbatim by `copy_nonoverlapping`).
>   - **`CRATONVM_DBG_FORCE_NONMOVING` "eliminating" the 0x4 is also an artifact**
>     — the non-moving sweep leaves the arena fragmented, so the POST walk
>     truncates at the same malformed header and never reaches the corrupt object
>     ("stale0x4=0" = "no longer walk-reachable", not "not written"). Under
>     FORCE_NONMOVING the test still FAILS — with `ArrayIndexOutOfBoundsException`
>     in `LongArray.reduceInPlace`/`modMultiply` — i.e. the *same* underlying
>     corruption, different surface.
>
> **Net:** the 0x4 is **written during normal BC EC execution** (it pre-exists the
> GC), bypassing `set_field`, `write_prim_element`, and all GC writes — exactly
> the "RAW cell write that bypasses the heap set API" the 2026-06-04 update below
> already concluded. The moving collector is **not** the corruptor; it is merely
> the only collector that makes the pre-existing corruption walk-visible. **This
> session's earlier "moving Cheney creates the 0x4" line of attack was wrong.**
>
> ### Open: the masking still has no found producer
> `0x4 = 0xFFFD000000000004 & PAYLOAD_MASK(47-bit)`. The ONLY code that applies
> that mask *and* builds an `ObjectRef` without the 8-byte-alignment degrade is
> `CompactValue::as_object_ptr()` (`types/src/compact_value.rs`) — and its only
> callers are the **GC frame/stack scan/update** (`value_stack.rs` / `frame.rs`),
> which write *frame* CompactValue slots, never heap fields. `to_value()`
> **degrades** an unaligned payload (4 % 8 ≠ 0) to `Value::Long`, so the
> operand-stack→`to_value()`→`putfield` path yields `Object(None)`, not
> `Object(0x4)`. So *how* a 47-bit-masked collision long reaches a heap field's
> offset+8 is **still unexplained** — this is the same wall the 2026-06-04
> "residual" section hit.
>
> ### Recommended next step (do this FIRST next session)
> Replace the verifier's `walk_objects` linear scan with a **graph traversal**
> (BFS from `collect_roots`, following `Value::Object` fields) OR run the verify
> **between GCs** (e.g. a counter in `safepoint_check`/`maybe_gc`), so the 0x4 is
> caught at/near the moment it is written with the live Java frame for context —
> the linear walk is structurally incapable of this and wasted most of this
> session. THEN instrument that write site. The fix almost certainly belongs at
> the value-model / operand-slot layer (per 2026-06-04), NOT in `gen_heap.rs`.
> All session diagnostics were reverted (tree = pre-session + the pre-existing
> `CRATONVM_DBG_BADREF`); re-add from this list as needed.

> ## ⚠️ RE-DIAGNOSIS 2026-06-04 (session "take it on") — the framing below is WRONG
> Two prior conclusions in this doc are **refuted** by fresh measurement (dev,
> uniquely-named binary `cratonvm_ecjit.exe` to dodge the cross-session
> `taskkill /F /IM cratonvm.exe` artifact; repro `-Xmx256m` forces GC and surfaces
> corruption in ~14 s):
>
> 1. **"needs many BC packages JIT-compiled together" is FALSE.** JIT-ing ONLY
>    `org/bouncycastle/math/ec/` (`CRATONVM_JIT_ALLOW_PACKAGES=org/bouncycastle/`
>    `CRATONVM_JIT_BISECT_ONLY=org/bouncycastle/math/ec/`) reproduces the heap
>    corruption / SEGV in 14 s. The prior round's "single-package bisection only
>    times out" was the taskkill artifact masking the corruption as a timeout.
>
> 2. **It is NOT (only) a JIT codegen miscompile.** `FixedPointTest` in isolation
>    FAILS **with JIT fully disabled** (`CRATONVM_DISABLE_JIT=1`): a flood of
>    `gen_heap::set_field: out-of-bounds field write dropped` (putfield index 0
>    into a bare `java/lang/Object`, `num_slots=0`, some with garbage `class_id`s
>    like 2121213 / 3114129) followed by a fatal
>    `expected object reference, got long(-844424930131964)` — i.e. a
>    **CompactValue long↔object NaN-box collision** reaching a *context-free
>    decoder* (`types/src/compact_value.rs`, `note_object_degradation` /
>    `object_degradation_count`). `-844424930131964 = 0xFFFD000000000004`: the top
>    bits are the quiet-NaN `SUB_OBJECT` tag, the payload is `4` (an invalid
>    pointer). Sometimes the bogus pointer (~4) passes a recovery path and is
>    dereferenced → `EXCEPTION_ACCESS_VIOLATION read at address 0x14` (= 4 + 0x10),
>    SEGV even with JIT off. The outcome (clean error / SEGV / heap-walker desync)
>    is nondeterministic, decided by whether the collided long happens to look like
>    a live heap address.
>
> **Real root cause:** a value-representation / operand-slot type-confusion in BC's
> EC arithmetic path. BC F2m `LongArray` (GF(2^m): `lxor`/`lshl`/`lushr` over
> `long[]`) produces 64-bit values whose bits land in the `SUB_OBJECT` NaN-box
> space (`0xFFFD…`); one such long ends up in a *reference-typed* slot and a
> context-free `to_value()` decode classifies it as an object (then degrades /
> derefs). The JIT ban only suppresses the JIT face of this; the interpreter face
> (`FixedPointTest` wrong result, `NISTECC: Exception`) is the same bug. The
> ongoing dated patches (`Round-8`, `BC SM2 2026-05-28 verbatim-long encode`) are
> in this same fight. cf. memory `reference_jca_synthetic_crypto_layers`
> ("interpreter long/float operand-stack bug").
>
> **Implication for the JIT ban:** lifting it is NOT the gating prerequisite the
> handoff assumed — the value-model collision must be fixed first (it breaks the
> interpreter too). And the RSA/AES native-accel lever is INDEPENDENT of this (those
> are pure interpreter *slowness*, not the EC collision) — it does not need the ban
> lifted or this bug fixed.
>
> Fast repro harnesses left in `/tmp`: `ecrun2.sh` (ALLOW/ONLY/SKIP/heap/timeout,
> classifies CORRUPT/SEGV/WATCHDOG/OK), `ectoggle.sh` (per-feature toggle).

> ## ✅ UPDATE 2026-06-04 (session "fix that bug") — three sound collision-long
> fixes landed; the **interpreter** (JIT-off) face is partially fixed but a
> residual GC-pressure corruption remains. Repro narrowed to JIT-off
> `FixedPointTest -Xmx256m` (corrupts ~14 s; passes 0-corruption at -Xmx1g/6g in
> 120 s — the corruption is **GC-frequency-driven**, confirming a GC/value-model
> bug, not a pure miscompile).
>
> ### Root mechanism confirmed
> A BC F2m `LongArray` word whose bits collide with the `SUB_OBJECT` NaN-box tag
> (`0xfffd_…`, e.g. `0xFFFD000000000004`) is bit-identical to
> `CompactValue::object(payload)`. When such a long sits in a slot the GC scans
> **without** a kind tag, the collector roots it, relocates the object its
> low-47-bits alias, and rewrites the slot — corrupting the long; or a
> context-free decode (`to_value`/`coerce_value_for_return`) turns it into a fake
> `Object(0x4)` / `null` reference that later NPEs (`getPoint`/`isZero`/`null
> array`/`null object argument`). The 2026-05-16 SoA collapse had dropped the
> per-local kind tag, which is what re-exposed this.
>
> ### Landed (sound; discriminating unit tests: frame 29, value_stack 54, dup 12)
> 1. **Locals kind tag restored.** `Frame` regains `local_kinds` (reuses the
>    frame-pool `Vec<u8>` the SoA collapse left unused). `scan_local_objects` /
>    `update_local_refs` skip `LKIND_LONG`/`LKIND_DOUBLE` slots — a primitive
>    long/double is NEVER a GC root/relocation target even when its bits look
>    like `SUB_OBJECT`. (`vm/src/runtime/frame.rs`.)
> 2. **Operand-stack scan/update made kind-strict.** `ValueStack::scan_object_refs`
>    no longer roots a `KIND_LONG`/`KIND_DOUBLE` slot via the old `is_heap_addr`
>    fallback (that branch only ever caught collision longs; the genuine JNI
>    long-as-jobject smuggle uses *low* addresses → untagged → handled by the
>    separate `CompactTag::Long|Double` branch). `update_object_refs` gains the
>    symmetric guard (the global `pointer_map` can match a collision long's
>    payload against an unrelated moved object). (`vm/src/runtime/value_stack.rs`.)
> 3. **Kind-preserving stack shuffles.** `push_compact_checked` lands
>    `KIND_UNKNOWN`, so the slow-path `dup`/`dup_x*`/`dup2*`/`swap`/`pop2`
>    (BC is non-JDK but these opcodes fall through to the slow path) were
>    **erasing** a collision long's `KIND_LONG` mark — defeating #1/#2 by making
>    the GC root it. New `ValueStack::{pop,peek,push}_with_kind` + `is_cat2_kind`
>    carry the kind across the shuffle and decide category-2 by the **kind**
>    (a collision long is `is_category2()==false` by bits). (`value_stack.rs` +
>    the `Instruction::Dup*/Swap/Pop2` handlers in `interpreter.rs`.)
>
> Effect: removes the `expected object reference, got long` errors and the
> original `getPoint on null`; the failure now surfaces deeper.
>
> ### Residual (NOT fixed) — collision-long → fake/null reference in **heap fields**
> Under -Xmx256m the test still fails. Instrumentation findings (all reverted):
> - Frame slots are clean post-GC (`CRATONVM_GC_VERIFY_STALE` empty). The
>   staleness is in **heap object fields/ref-arrays** (`CRATONVM_DBG_HEAP_STALE`:
>   `X9ECParameters/ECCurve$F2m/Level/ThreadLocalMap$Entry field[i] -> 0x4
>   OFF-HEAP`). `0x4` is exactly the unaligned payload of `0xFFFD…0004`.
> - The fake `0x4` is **OFF-HEAP** (`is_heap_addr(0x4)==None`) — so it is **not**
>   a relocation/forwarding artifact (the GC only ever deals in heap addresses)
>   and the GC root scan never roots it (already rejected by `is_heap_addr`). It
>   is a value WRITTEN verbatim into a `Value::Object` field cell.
> - It is **not** written via any bytecode `putfield`/`aastore`/`getfield`/
>   `aaload`, nor at invoke receiver/arg decode (all instrumented, all silent).
> - **DEFINITIVE (correlated test):** the `gen_heap::set_field` AND
>   `set_array_element` bad-value checks (`Value::Object(Some(p))`,
>   `0 < p < 0x10000`, gate `CRATONVM_DBG_BADREF`) stayed **silent in two runs
>   that DID surface `0x4`** (8 and 2 instances; one run SEGV'd on the `0x4`
>   deref). So the `0x4` is written by a **RAW cell write that bypasses the entire
>   heap set API** — excluding ALL bytecode, native `ctx.set_field`, AND
>   reflection (those all funnel through `set_field`/`set_array_element`). The
>   diagnostics are LEFT IN PLACE (gated) for the next investigator.
> - Every GC field path (Cheney scan, dirty-card old→young, mark scan, old-gen
>   `compact()`) reads/writes the 16-byte `Value` and only touches
>   `Value::Object` — type-safe; `forward_object` rejects unaligned forwarding
>   pointers. So the heap-field corruption is **not** a GC field-walk type
>   confusion. Heap object fields are 16-byte `Value` (explicit tag), so a `long`
>   field cannot be misread as `Object`.
>
> ### Next steps (where to instrument)
> - **Primary (raw-write hunt):** the `0x4` enters via a raw `*mut Value` cell
>   write (NOT the set API). Two candidates remain: (a) a GC raw write
>   (`std::ptr::write(s_ptr as *mut Value, …)` in gen_heap Cheney/dirty-card or
>   old_gen `compact`) — instrument those WRITE sites for `Object(Some(p<0x10000))`
>   AND the READ sites to tell "GC creates `0x4`" from "GC copies a pre-existing
>   `0x4`"; (b) a native **Unsafe**-style direct memory write (`putObject` / arena
>   handles / `copy_to_native_memory`) that bypasses `ctx.set_field`. Decide
>   between them with a **pre-GC full-heap scan** (call `verify_heap_object_fields`
>   at GC ENTRY with an empty map): if `0x4` already exists pre-GC → it is written
>   during normal execution (native raw write / construction); if only post-GC →
>   the GC creates it.
> - Secondary (the surviving operand-stack kind-loss): `collect_roots`'s
>   `is_object_address` re-filter PASSES a zeroed `num_slots=0/class_id=0` header,
>   so a `KIND_UNKNOWN` collision long aliasing zeroed young memory can still be
>   rooted/moved — log when the re-filter keeps such a header. Audit the other
>   ~40 `push_compact_checked` callers and the cached (non-slow-path) invoke arg
>   collection. Note: the concurrent `feat/precise-jit-stack-maps` merge
>   (shadow-stack precise JIT roots, gated default-OFF) is the JIT-side analogue
>   of this same context-aware-roots fix.
> - Candidate clean fix once located: same kind-preservation, or have
>   `collect_roots`'s re-filter reject zeroed-header candidates for non-`KIND`
>   slots.

## Reproduction (existing release binary, no rebuild needed)

```
cd apps/_test-suites/bc-java
CRATONVM_JIT_ALLOW_PACKAGES='org/bouncycastle/' \
  target/release/cratonvm.exe --java-home "C:/Program Files/Java/jdk-25" \
  -Xmx1g -cp "core/build/classes/java/main;core/build/classes/java/test;\
core/build/resources/main;core/build/resources/test;$TEMP/junit-3.8.2.jar" \
  junit.textui.TestRunner org.bouncycastle.math.ec.test.AllTests
```

Deterministic crash:

```
EXCEPTION_ACCESS_VIOLATION (0xC0000005) at pc=0x000000004D400029
Faulting access: read at address 0x0000000000000031
rax=1 rcx=<heap obj> rdx=1 rbx=0x000000004D400000 ... rip=rbx+0x29
```

`HEADER_SIZE=40 (0x28)`, `FIELD_CELL_PAYLOAD64_OFFSET=8` ⇒ field-0 payload is
at `obj+0x30`. The fault reads `[1 + 0x30] = 0x31`: **a small integer `1`
(or `3`, reading array-length at `[3+0xC]=0xF` with inline-getfield disabled)
is sitting in a slot consumed as an object/array reference.** It is later
dereferenced by an inline `getfield`/array op, and/or followed into a bad
pointer by the young-gen scavenge copy loop (frame-2 GC `movups` field-cell
copy + `and [rdx+0xD0],0x100000` flag mask in the backtrace).

## What was RULED OUT (all still crash identically)

- `CRATONVM_DISABLE_SCALAR_REPLACEMENT=1` — still crashes ⇒ NOT scalar
  replacement.
- `CRATONVM_JIT_DISABLE_INLINE_NEW=1` — still crashes ⇒ NOT the inline TLAB
  `new` path.
- `DISABLE_INLINE_GETFIELD=1` — crash *moves* (now an array op, base `3`) but
  persists ⇒ the inline getfield is only a *dereference site*; the bad value
  is produced upstream.
- `dup2` (0x5C): a full disassembly scan of the entire `org/bouncycastle/math`
  tree found **no FORM-2 `dup2`** (every `dup2` is on `[arrayref,int]` — two
  category-1 values). So `dup2` is not the EC trigger (though it *is* a real
  latent miscompile — fixed below as defense-in-depth).
- Package bisection (`CRATONVM_JIT_BISECT_ONLY` / `CRATONVM_JIT_BISECT_SKIP`):
  no single BC sub-package (`math/`, `util/`, `asn1/`, `crypto/`, `internal/`,
  `test/`, …) reproduces in isolation — they all just time out. Only the FULL
  `org/bouncycastle/` allow-set crashes. ⇒ the miscompile needs **many
  packages JIT-compiled together** (a cross-package JIT→JIT direct-call / PIC
  interaction), consistent with the symptom (a primitive `1` landing in the
  callee's receiver/arg register `rdx`).

## Most likely remaining root cause (unconfirmed)

An **argument-marshalling / dispatch miscompile on a cross-package JIT→JIT
call**: the caller loads the wrong operand-stack slot (a primitive `1`) as the
receiver/first arg. The faulting callee is entered with `rdx=1` (Win64
ARG_REGS[1] = receiver for a needs-context call) and immediately does
`getfield field-0` on it. Suspects to audit next (needs a build to iterate):
- the direct-call / sibling-tail / PIC arg-slot marshalling around
  `jit/src/x64.rs:14609+` and `:15890+` (receiver/arg slot ordering, esp. when
  a category-2 arg occupies one JIT stack entry but the JVM descriptor counts
  two slots);
- operand-stack oop-mark propagation in `dup`/`dup2`/`swap`
  (`jit/src/x64.rs:11768+`): the `CalleeSaved`/`Xmm` arms push to `self.stack`
  WITHOUT a matching `self.stack_oop_marks` push, desyncing the oop-mark
  vector (the `emit_oop_map_for_safepoint` "lazy resync" pads at the END,
  mis-attributing marks). This is a real latent bug; whether it produces the
  EC value-corruption is unconfirmed.

To pinpoint: add a faulting-RIP code-bytes dump to the crash handler
(`vm/src/runtime/crash_handler.rs` — out of scope for the JIT agent) so the
exact miscompiled instruction at `0x4D400029` can be disassembled.

## Fixes that DID land (jit/src/x64.rs only)

1. **Escape-analysis / scalar-replacement control-flow soundness.**
   `analyze_escapes` and `plan_scalar_replacement` are single LINEAR passes
   that carried abstract operand-stack + per-local provenance straight through
   branches with no per-block reset/merge — unsound for any method with
   control flow (the documented "allocate-then-putfield" class). Added a hard
   provenance barrier at every control-transfer **source** (each branch /
   switch / goto / throw / ret arm) and every branch **target**
   (`compute_branch_targets`), confining scalar replacement to objects whose
   whole `new; dup; <init>()V; (putfield|getfield)*` lifecycle is within one
   straight-line region. (Did not fix the EC crash, but is a correct fix for a
   real miscompile class.)

2. **`dup2` (0x5C) category-2 guard** (`dup2_category_safe`, gating in
   `jit_scan`). The codegen `dup2` handler unconditionally implements the
   two-category-1 form; for a single category-2 (long/double) operand it
   duplicates an unrelated lower slot, desyncing the stack. New analyzer
   rejects (interpreter fallback) only the provable FORM-2 case; the common
   `arr[i] op= x` FORM-1 `dup2` still JITs. (Sound; not the EC trigger since EC
   has no FORM-2 dup2.)

## Ban status

`vm/src/jit/skip_list.rs:457-461` blanket `org/bouncycastle/` ban is
**unchanged**. Do not remove until the cross-package dispatch miscompile above
is root-caused and fixed, or the EC AllTests run completes cleanly under
`CRATONVM_JIT_ALLOW_PACKAGES='org/bouncycastle/'`.
