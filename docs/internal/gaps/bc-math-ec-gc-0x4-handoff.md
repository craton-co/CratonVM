# bc-math-ec `0x4` heap corruption — full handoff (2026-06-05)

> # ✅ RESOLVED 2026-06-10 (branch fix/bc-math-ec-gc-0x4)
> **Root cause: ReferenceProcessor re-emission** — every cleared/enqueued/
> cleaner action was re-emitted on EVERY GC forever; once the Reference died,
> its recycled registry address aliased innocent objects and the per-GC
> referent-null / enqueue writes corrupted them through validly-remapped
> addresses. On-grid hits = silent legal nulls (the BigInteger NPEs);
> interior hits = the mis-gridded 16-byte `Object(None)` `{disc=4,payload=0}`
> = the `Object(Some(0x4))` signature this doc chronicles. NOT a collector
> write — the writes happen in the post-GC reference-processing window,
> which is what fooled the GC-EXIT detector ("fact 4").
> **Fix:** exactly-once emission (gc/src/reference.rs `take_newly_cleared` /
> drained `pending_queues` + `finalization_queue` / cleaner `action_emitted`)
> + dead-Reference `is_stale_young` guards in `process_references_after_gc`.
> **Validated:** FixedPointTest -Xmx128m 8/8 OK default-mode (was 0/40+);
> regression pool zero regressions, commons-math-junit-probe FAIL→PASS.
> Full causal chain + evidence: `docs/internal/h2-testscript-segv-findings.md`.
> Everything below is the historical hunt log.

> ## ⚡⚡ SESSION UPDATE 2026-06-05 ("take on this job", worktree `CratonVM-ecgc`,
> branch `fix/bc-math-ec-gc-0x4`) — **THE "GC WRITES THE 0x4" FRAMING (this
> doc's headline + fact 4) IS WRONG. The `0x4` is a MUTATOR write to a YOUNG
> object; the GC merely PROMOTES the already-corrupt object** (which is why
> `ec_watch`'s GC-EXIT *saw* it — a promotion/post-GC-step artifact, not a
> collector write). The earlier sessions' "mutator raw write / BADREF-silent"
> conclusion was RIGHT; the "breakthrough" overturned it on bad evidence.
>
> ### PROVEN this session (new gated tools `CRATONVM_DBG_SEEDHUNT`,
> `..._YOUNGSCAN`, `..._STRAYSTACK` — all default-OFF, in the tree):
> 1. **`0x4` is present in YOUNG at GC ENTRY** (`CRATONVM_DBG_RSET_AUDIT`'s
>    `[small4] PRE-GC YOUNG … fld[i] -> 0x4`: HexFormat fld[1], cid724 fld[8],
>    **ECFieldElement$F2m fld[3]** — victim spans Fp AND F2m, varies per run).
> 2. **`CRATONVM_DBG_SEEDHUNT`** (phase-bisect in `collect_garbage_inner`): the
>    seeding GC is always `major_ran=false`, no old-gen jump, no `[gcfwd]` → it
>    is NOT major-GC `update_refs_in_object` (§6.1 REFUTED) and NOT any minor
>    collector write. The promoted-out victim just carries a pre-existing young
>    `0x4` (Cheney copy is verbatim — fact 6).
> 3. **value `4` == the `Value::Object` discriminant.** The OOB-flood objects
>    read `class_id ∈ {0,1,4}` (== Int/Long/Object discs) with `num_slots=0` —
>    a header read at a relocated-but-unremapped / interior address.
> 4. **`set_field` is EXONERATED for the `0x4`**: its guards DROP a stray target
>    (num_slots=0 → OOB-drop; forwarded real+8 → num_slots=fwd_low>1<<24 →
>    suspect-header drop). The `0x4` victim has an INTACT header → it's a RAW
>    write to a VALID object's field PAYLOAD (off-by-8 within the 16-byte cell,
>    disc-of-victim untouched), bypassing set_field, BADREF, AND the write
>    barrier. `Unsafe.putObject` (→ ctx.set_field), natives (no raw Value
>    writes), GC (pre-GC), reference-processing — ALL exonerated for the `0x4`.
>
> ### FIXED this session (real, verified bug — but NOT the `0x4`):
> **Reference-processing stale-ref writes** (`interpreter.rs`
> `process_references_after_gc`, ~637-680). It remaps `cleared`/`to_enqueue`
> Reference addrs via `pointer_map.get(a).unwrap_or(*a)`; a Reference RECLAIMED
> this cycle keeps its stale PRE-GC addr and `set_field`s stray into the reusing
> object — the `set_field out-of-bounds` flood. `q_obj` was liveness-guarded
> (avrora fix); **`ref_obj` (671/672/679) and cleared `obj_ref` (642) were
> NOT.** Added `num_fields(x) < 2 → skip` guards (mirror q_obj). Verified:
> `[refproc] SKIP` fires 12×, OOB-flood `1-13 → 0`. EC corruption PERSISTS, so
> this is a distinct (now-fixed) bug, NOT the `0x4`.
>
> ### NEW LEAD for the `0x4` (chase FIRST next session):
> **A remembered-set MISS** appeared (`[rset-audit] … MISSES=1`, contradicting
> this doc's fact 3 of MISSES=0): `OLD X9ECParameters@0x1aa0e488 fld[0] -> young
> 0x24981000 CLEAN(card)` — an old→young edge with an undirtied card, in the
> SAME young region (0x24981xxx) as that run's `0x4` victim (F2m@0x24981470).
> A CLEAN card on a real old→young edge == the field was written by a path that
> **bypassed the write barrier** — i.e. the SAME raw-write that produces the
> `0x4` (off-by-8) produces the rset-miss (old→young, no card). Find the raw
> heap-cell write that skips `set_field`/`write_slot`'s barrier. Candidates not
> yet instrumented: object-construction field-init for OLD-gen/humongous allocs;
> `alloc_object_with_descriptors`; any `write_slot`/`ptr::write::<Value>` caller
> outside `set_field`. A **hardware watchpoint** on a young victim's payload
> (re-arm on recurrence, since it persists+recurs) remains the definitive tool —
> the software youngscan RACES the SEGV (rc=139) and the promotion.
>
> Tools added (gated, default-OFF): `CRATONVM_DBG_SEEDHUNT` (gen_heap),
> `CRATONVM_DBG_YOUNGSCAN` (vm_exec safe_native_call, brute-force young
> `{disc=4,payload<0x1000}` + back-validate), `CRATONVM_DBG_STRAYSTACK`
> (interpreter putfield + NativeContextImpl::set_field stray-receiver Java
> stack + the `[refproc] SKIP` log), `[gcfwd]` (old_gen §6.1 check). Repro
> scripts in `ecprobe/`. Build traps: vm crate (lto=fat) compile is
> intermittently OOM/load-killed → exit 1 w/ no error after the gc warning;
> just RETRY. Verify relink by binary MTIME (comments are stripped).
>
> **Everything BELOW this block predates the reframe — its "GC writes it" /
> §6.1 / fact-4 conclusions are SUPERSEDED. Read it for the ruled-out list and
> the repro only.**

---

**Status: ROOT CAUSE LOCALIZED to the moving collector, exact write-line still open.**
This is a complete brain-dump for the next agent. Read it before touching anything —
it captures what is PROVEN, what is RULED OUT (don't redo), the diagnostic tools
that are already in the tree (gated, default-OFF), and the prioritized next steps.

Companion (older, partly superseded) notes:
`docs/bc-math-ec-jit-miscompile-investigation.md` (has the dated session log + the
`⚡ BREAKTHROUGH` block at the top that matches this doc).

---

## 0. TL;DR

`org.bouncycastle.math.ec.test.FixedPointTest` (JIT off, `-Xmx256m`) corrupts EC
heap objects: a reference field that held a valid pointer becomes
`Value::Object(Some(0x4))` (discriminant = Object, payload = `0x0000_0000_0000_0004`).
The bad ref is later dereferenced → wrong EC point / `NullPointerException` / SEGV.

**A working software watchpoint (`CRATONVM_DBG_ECWATCH`) PROVES the value is written
DURING `collect_garbage` (the minor moving Cheney GC), NOT by the mutator** — and the
hit is **class-id-validated**, so it is a real corruption of the watched object, not a
remap artifact.

**But every concrete minor-GC write is ruled out**: `forward_object` never returns a
small (`<0x1000`) address, and the object copy only PROPAGATES a pre-existing `0x4`
(never creates one). So the per-collect SEED of a new `0x4` is a write whose exact site
I have not yet pinned. Leading structural clue: **`Value`'s `Object` discriminant == 4
== `ClassId(4)` == `java/lang/constant/Constable`, and both live at byte offset 0**
(of a 16-byte `Value` cell / of an `ObjectHeader`) — a header/field aliasing is the
prime suspect.

This is NOT: F2m `LongArray` collision longs (the old framing — wrong), a mutator raw
write, an old→young remembered-set miss, `set_field`/`set_array_element`,
`copy_to_native_memory`, or a primitive-array OOB. All measured-and-excluded — see §5.

---

## 1. Reproduction (existing release binary; no rebuild needed)

```bash
cd /c/craton/CratonVM/apps/_test-suites/bc-java
CP="core/build/classes/java/main;core/build/classes/java/test;core/build/resources/main;core/build/resources/test;$TEMP/junit-3.8.2.jar"
# Use a UNIQUELY-NAMED binary to dodge the cross-session `taskkill /F /IM cratonvm.exe`
# artifact (rc=1/empty output is NOT a real failure — it's another session killing it).
cp -f /c/craton/CratonVM/target/release/cratonvm.exe /c/craton/CratonVM/target/release/cratonvm_ecjit.exe
CRATONVM_DISABLE_JIT=1 CRATONVM_DBG_HEAP_STALE=1 \
  /c/craton/CratonVM/target/release/cratonvm_ecjit.exe \
  --java-home "C:/Program Files/Java/jdk-25" --stack-dump-on-timeout 0 -Xmx256m -cp "$CP" \
  junit.textui.TestRunner org.bouncycastle.math.ec.test.FixedPointTest
```

- Corrupts on EVERY run (~50–115 s). Sometimes ends `Failures: 1` (wrong point),
  sometimes `Errors: 1` (NPE `Cannot invoke isZero on null`), sometimes SEGV (rc=139,
  faulting read at `0x14` = `4 + 0x10`, deref of the `0x4` ref).
- **GC-frequency-driven**: clean at `-Xmx1g`/`-Xmx6g` (the doc's older measurement).
  More GC pressure → more corruption. Pin: the collector, under pressure.
- `--stack-dump-on-timeout 0` disables the 120 s watchdog so a slow/instrumented run
  doesn't abort itself (rc=127).

`FixedPointTest` iterates EVERY named curve (`ECNamedCurveTable` + `CustomNamedCurves`)
× 5, comparing `ECAlgorithms.referenceMultiply` vs `FixedPointCombMultiplier.multiply`.
Both Fp and F2m curves run. **The corruption is in Fp/SecP192R1-class objects** mostly
(`X9ECParameters`, `X9Curve`, `ECCurve$Fp`, `ECPoint$Fp`, `ECFieldElement$Fp`) — but the
victim is essentially RANDOM (see §4): across runs it is also `SecT*Point`,
`java/util/HexFormat`, `java/util/logging/Level`.

---

## 2. The diagnostic tools (already in the tree, gated default-OFF, zero cost when unset)

All in `dev`. Turn on with the env var; combine freely.

| Env var | Where | What it does |
|---|---|---|
| `CRATONVM_DBG_ECWATCH` | `vm/src/runtime/ec_watch.rs` (+ Putfield hook in `interpreter.rs`, GC hooks in `maybe_gc`/`maybe_gc_forced`/`force_gc_from_native`) | **The software watchpoint.** On `Putfield` of a *valid* ref into a watched EC holder (class name has `/asn1/x9/` or `Curve`), records `(ObjectRef, field_idx, expected_ptr, class_id)`. Detects (discriminant-checked + class-id-validated) any watched cell that flips to a non-zero `<0x1000` `Object` value. **Detect points: GC-ENTRY** (`[ecwatch-GC]`), **GC-EXIT** (`[ecwatch-GCEXIT]`, right after `collect_garbage`+remap), and per-native if `..._NATIVE` is set. **REMAPS holders through the GC `pointer_map` on every collect** (`ec_watch::remap`) — the critical design point; an earlier clear-on-GC version caught nothing because the corruption hits objects that SURVIVE the GC that wrote them. |
| `CRATONVM_DBG_ECWATCH_NATIVE` | `vm/src/vm/vm_exec.rs` `safe_native_call` | Re-runs `ec_watch::detect` after EVERY native dispatch and prints the native + Java stack. EXPENSIVE (O(watch-list) per native). Blames the native whose ALLOCATION triggers the GC, not the writer — use the STACK, not the name. |
| `CRATONVM_DBG_GCWRITE` | `gc/src/gen_heap.rs` | Two checks: (a) after `copy_nonoverlapping` in `forward_object`, compares each copied object's dest fields vs source and prints `[gcwrite] COPY fld[i]=0x.. src_small=<bool> cid=.. num_slots=.. total_size=..`; (b) a `forward_object` return-value wrapper printing `[gcwrite] forward_object RETURNED 0x..` if it ever returns `<0x1000`. |
| `CRATONVM_DBG_HEAP_STALE` | `vm/src/memory/gc.rs` `verify_heap_object_fields` | Post-GC heap walk; prints `[heap-stale] OFF-HEAP(reclaimed)/ZEROED(reclaimed)/UN-FORWARDED OBJ <class> field[i] -> 0x..`. INDEPENDENT of the watchpoint — confirms the `0x4` in live objects. NOTE its young linear `walk_objects` truncates at a bad header (old-gen is robust). |
| `CRATONVM_DBG_RSET_AUDIT` | `gc/src/gen_heap.rs` `collect_garbage_inner` | Pre-GC remembered-set audit + `[small4]` scan of young+old for `Object(<0x1000)` fields. `MISSES=0` proved the remembered set is correct. |
| `CRATONVM_DBG_BADREF` | `gc/src/gen_heap.rs` `set_field`/`set_array_element` | Fires if a `<0x10000` ref is stored via the heap set API. **Stays SILENT while `0x4` surfaces** → the write bypasses `set_field`/`set_array_element`. |
| `CRATONVM_DBG_HEAPCOPY` | `vm/src/vm/vm_exec.rs` `copy_to_native_memory` | Fires if a raw native memory copy targets a managed-heap address. **0 hits** → exonerated. |

Older one-shot diagnostics referenced in the investigation doc (`CRATONVM_DBG_FORCE_MOVING`,
`CRATONVM_DISABLE_JIT`, etc.) still exist. NOTE `CRATONVM_DBG_FORCE_NONMOVING` is a NO-OP
with JIT off (quiescence inactive ⇒ moving Cheney runs regardless; the flag is only
checked when quiescence is active) — don't waste a run on it.

---

## 3. CONFIRMED facts (each measured this session, with the gated tool)

1. **The corruption is `Value::Object(Some(0x4))` in a heap reference field** — disc =
   Object (variant 4), payload = `0x0000_0000_0000_0004` (high-32 zero because the heap
   is < 4 GiB). `HEAP_STALE` reports it; the bad ref is dereferenced → SEGV at `0x14`.
2. **It bypasses the heap set API.** `BADREF` is silent in runs that surface `0x4`.
   `Putfield`/`Aastore` both funnel through `set_field`/`set_array_element`; `coerce_value_for_return`
   can only yield `Object(None)` or a *huge* ptr from a `Long`, never `0x4`.
3. **The remembered set is correct** (`RSET_AUDIT` → `MISSES=0` every GC). NOT an
   old→young write-barrier miss.
4. **THE GC WRITES IT.** `CRATONVM_DBG_ECWATCH` GC-EXIT detector: a watched cell that is
   clean at GC ENTRY reads `0x4` immediately after `collect_garbage`
   (`[ecwatch-GCEXIT] holder@.. fld[i]: 0x<valid> -> 0x4 corrupted DURING collect_garbage`).
   **Class-id-validated** (the holder's header `class_id` still matches the watched class),
   so it is NOT a remap false-positive — it is a real corruption of the real watched object.
5. **`forward_object` never returns `<0x1000`** (the wrapper fired 0×). So NONE of the
   minor-GC reference-update writes (all of which write `Object(Some(from_raw(forward_object(..))))`
   or `forward_object(..) as u64`) can produce `0x4`.
6. **The object copy only PROPAGATES `0x4`, never CREATES it.** Every `[gcwrite] COPY`
   has `src_small=true` (the from-space SOURCE already held the `0x4`); `copy_nonoverlapping`
   is verbatim. So this GC's to-space `0x4` becomes next GC's from-space `0x4` — that's why
   it looks self-sustaining, but the per-collect SEED is a separate NEW write (fact 4).
7. **`bi_alloc_int` had a genuine native use-after-move** (allocated the `BigInteger`,
   then `new_array(mag)` could GC-relocate the not-yet-rooted object, then `set_field`
   through the stale ref). FIXED on `dev` with `ctx.pin_native_root`/`read_native_pin`.
   This was a REAL bug (it caused the `set_field` out-of-bounds flood) but is **NOT** the
   `0x4` writer (its writes are `Int(signum)`/`Object(mag)` to fld[0]/[1], never `0x4`).
8. **Victim is random / long-lived.** Across runs the corrupted holder is
   `X9ECParameters` / `X9Curve` / `ECCurve$Fp` / `ECPoint$Fp`, but also
   `SecT163K1Point`, `SecP128R1Curve`, `java/util/HexFormat`, `java/util/logging/Level`.
   The corrupted field index VARIES (1, 2, 3, 4, 7, 9, 10). ⇒ the write targets an
   address that different long-lived objects occupy over time.

---

## 4. The leading STRUCTURAL clue (chase this first)

- `Value` (16 bytes) discriminant is at **byte offset 0**; for the `Object` variant it
  is **4** (Int=0, Long=1, Float=2, Double=3, **Object=4**, ReturnAddress=5, Uninit=6).
  The payload pointer is at **offset +8**.
- `ObjectHeader` (`#[repr(C)]`, `types/src/heap_types.rs`) has **`class_id: ClassId(u32)`
  at byte offset 0**, then `kind`(4), `element_type`(5), pad(6-7), `identity_hash_code`(8-11),
  `array_length`(12-15), `num_slots`(16-19), …, `forwarding_ptr`(24), `mark_word`(32).
- **`ClassId(4)` == `java/lang/constant/Constable`** (seen in the `set_field` OOB flood
  logs: `class_id=ClassId(4) class_name=java/lang/constant/Constable`).

So a heap location interpreted BOTH as an `ObjectHeader` (class_id=4 at offset 0) AND as
a `Value` cell (disc at offset 0) reads as `Value::Object` with disc 4. **The `0x4`
corruption == disc 4 (Object) + payload 4 is exactly what you get if a small/`class_id=4`
header-shaped value is written over a Value field cell, OR a field cell is read/written
at an object-header offset.** Prime hypotheses (next section) all reduce to a
header↔field-cell aliasing inside the collector under promotion/compaction pressure.

(Payload = 4 specifically — note `array_length`/`num_slots`/`gc_age` are small integers
that live at header offsets 12/16/22; one of them landing on a field's `+8` payload-low
would give a small payload. `identity_hash_code` at +8 would give a LARGE payload, so the
"plain Constable header overlaid on the cell" variant gives `Object(large)` not
`Object(4)` — the exact byte alignment of the aliasing matters; instrument to learn it.)

---

## 5. RULED OUT — do NOT redo these

- F2m `LongArray` GF(2^m) collision longs (`0xFFFD…`). The whole "NaN-box collision long
  written into a ref cell" framing from the older sessions is a DEAD END for this test —
  the victims are Fp/asn1.x9, and `0x4` is unaligned so no `to_value`/`decode_value`/
  `as_object_ptr` path makes it. (Those alignment-degrade or are frame-only + gated.)
- A **mutator** raw write. `BADREF` silent + `forward_object` can't make it + the GC-EXIT
  detector proves it appears DURING collect. The older `[small4] PRE-GC` "mutator" reading
  was a PRIOR GC's output being seen at the next GC's entry.
- `set_field` / `set_array_element` (BADREF), all bytecode `putfield`/`aastore`/`Xastore`,
  `Unsafe.put{Int,Long,Object}` (route through set API or off-heap arena),
  `copy_to_native_memory` (HEAPCOPY = 0), the bulk byte/char array-region intrinsics
  (bounds-checked + correct stride), the `BigInteger` `impl*ToLen`/`mulAdd` intrinsics
  (bounds-checked `set_array_element`), `alloc_array` undersizing (reserves exactly
  `HEADER + len*elem`, sets `array_length=len`).
- Old→young remembered-set / write-barrier miss (`RSET_AUDIT` MISSES=0).
- `forward_object` returning a small value (wrapper 0×).
- The object copy CREATING `0x4` (all `src_small=true`).
- Watchpoint remap false-positive (class-id re-validation still fires).

---

## 6. Recommended next steps (prioritized)

The write is inside `collect_garbage` (gen_heap.rs `collect_garbage_inner`), produces a
16-byte `Object(disc=4, payload=4)` (or a small write that yields it), is NOT
`forward_object`'s value, and is NOT the verbatim copy. So it is a **wrong-target / raw
write** in one of the collector paths I have not instrumented:

1. **Old-gen / major-GC compaction `update_references`** (`gc/src/old_gen.rs:~629`):
   writes `Value::Object(Some(forwarding_ptr))` where `forwarding_ptr` is read from the
   *referent's header* (NOT `forward_object`). A stale/corrupt `forwarding_ptr` that is
   non-null but `<0x1000` (passes the `!is_null()` guard) writes `Object(small)`. This
   is the ONE `Value::Object` write that does NOT go through `forward_object`. It only
   writes OLD referrers — but `collect_garbage` runs a **major GC when old-gen ≥ 75%**
   (`gen_heap.rs` Phase 5), which fits the heap-pressure correlation, and promoted EC
   objects land in old-gen (the `[gcwrite] COPY` hits were promotions to `0x1aa1…`).
   ACTION: gate-log `forwarding_ptr < 0x1000` in that loop and in `major_gc` /
   `gc/src/old_gen.rs` `compact()`; also instrument `forwarding_ptr` install sites for a
   small value.
2. **Catch the exact write with a hardware/byte watchpoint.** When `set_field` stores a
   valid ref into a watched cell, you have its ADDRESS. Set a Dr0 hardware write
   breakpoint on that 8-byte payload (the VEH infra exists in
   `vm/src/runtime/crash_handler.rs`) — the faulting RIP IS the write instruction.
   This is the definitive tool for "who writes 4 to address A". (I did not build it; it's
   the highest-confidence path.)
3. **Header↔field-cell aliasing audit** (per §4). In `forward_object`, after the copy and
   header writes (`forwarding_ptr=null`, `gc_age+=1`, `gc_flags|=`, mark_word store), and
   in `young_to.alloc`/`old_gen.alloc`, assert the new object's `[ptr, ptr+total_size)`
   does NOT overlap any other live object (an allocator/free-list bug returning an
   overlapping address would put a header on a neighbour's field cell). The `class_id=4`
   (Constable) coincidence strongly suggests a header is being read/written as a field.
4. **Bisect the phase.** Add a young_to scan (`Object(<0x1000)` fields) after Phase 1b,
   after Phase 2 (Cheney), after Phase 2b (promoted), after Phase 3 (reset), and after
   `major_gc` — see which phase first introduces a NEW `0x4` on a previously-clean cell.
   (The watch-list isn't reachable from `gen_heap`; scan the arena directly.)
5. Confirm any fix with `CRATONVM_DBG_ECWATCH` (GC-EXIT count → 0) AND a clean
   `FixedPointTest` (no `Failures`/`Errors`, no SEGV) across ~8 runs.

---

## 7. Build & process traps (these cost a LOT of time — read them)

- **Build foreground, undisturbed.** Launching multiple background `cargo` builds leaves
  **orphaned cargo processes** that contend on `target/` → every later build dies at the
  cli link with a **silent `exit 1`** (no compile error in the log). Fix: kill ALL
  `cargo`/`rustc`/`rust-lld` to 0, then run ONE build to completion.
- Build command (PowerShell, the repo's `build-cpu.bat` does vcvars + `cargo build
  --release -p cratonvm-cli --bin cratonvm`):
  ```powershell
  1..3 | % { Get-Process cargo,rustc,rust-lld,link,cratonvm* -EA SilentlyContinue | Stop-Process -Force -EA SilentlyContinue; Start-Sleep -Milliseconds 500 }
  cmd /c "cd /d C:\craton\CratonVM && .\build-cpu.bat > build.log 2>&1"
  ```
  `NoDefaultCurrentDirectoryInExePath=1` on this box ⇒ invoke the batch as `.\build-cpu.bat`.
  A full build is ~6 min (gc-crate change rebuilds gc+vm+cli). A vm-only change is faster.
- **The `cmd /c` wrapper sometimes reports `exit 1` even on success** (a post-link probe).
  VERIFY the binary instead: `grep -c '<a unique string from your edit>' target/release/cratonvm.exe`
  and check the mtime/size advanced. (Stale-binary trap: a running `cratonvm*.exe` locks
  the file so `cargo` "succeeds" without relinking — kill them first.)
- **Run with a uniquely-named binary** (`cratonvm_ecjit.exe`, a copy) so a cross-session
  `taskkill /F /IM cratonvm.exe` (which produces rc=1/empty) can't be mistaken for a real
  failure. Kill stray `cratonvm*` before each run so they don't lock the copy target.

---

## 8. State of the tree (branch `dev`)

- Committed (`08b4adb` "BC math EC JIT miscompile investigation" + the `bi_alloc_int` fix):
  all gated diagnostics in `gc/src/gen_heap.rs`, `vm/src/vm/vm_exec.rs`,
  `vm/src/runtime/interpreter.rs` (Putfield hook + GC-entry/exit detect + remap calls),
  `vm/src/runtime/mod.rs` (`pub mod ec_watch;`), and the `bi_alloc_int` pin/remap fix in
  `native-builtins/src/lib.rs`.
- `vm/src/runtime/ec_watch.rs` — the watchpoint module (now `git add`-ed; was untracked).
- `forward_object` was split: `forward_object` (thin wrapper with the GCWRITE return check)
  → `forward_object_impl` (the original body). Behaviour is identical when the gate is off.
- All diagnostics are env-gated and default-OFF → **zero cost / behaviour-neutral** in a
  normal build. The `org/bouncycastle/` JIT ban in `vm/src/jit/skip_list.rs` is unchanged.
- Scratch repro scripts: `ecprobe_tmp/repro_nojit.sh`, `ecprobe_tmp/run_rset_audit.sh`.

## 9. One-line orientation for the takeover

Run the repro with `CRATONVM_DBG_ECWATCH=1 CRATONVM_DBG_GCWRITE=1` a few times until
`[ecwatch-GCEXIT]` fires; that is the moment the minor GC writes `0x4` to a real EC
object. Then instrument the **old-gen/major-GC `forwarding_ptr` writes** (§6.1) and/or set
a **hardware watchpoint** on a watched cell's payload (§6.2). The `Object`-disc == `Constable`
-class_id == 4 coincidence (§4) says you're looking for a **header-as-field-cell** write.
