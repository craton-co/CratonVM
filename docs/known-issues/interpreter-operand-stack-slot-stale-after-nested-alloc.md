# Stale refs surfacing on interpreter operand stacks after a nested allocating call — the residual core of the WildFly cid=0 family

Status: **RE-CHARACTERIZED 2026-07-23 (round 2); fatal member FIXED (producer #11); dominant
non-fatal producer FIXED (producer #12, `6e10cba68`) — residual tail SPLIT into THREE DISTINCT
open items (see "Campaign 13/fix11" section below), none yet fixed: (A) the original sb-chain
shape (unchanged, now pushprov-exhausted for every interpreter push site), (B) a NEW non-moving
young-gen liveness/remembered-set gap (FATAL — `AbstractMethodError`), (C) a NEW STW-takeover-
correlated checkcast CCE (FATAL — `ClassCastException`). All three are rare (<1%/boot) but (B)
and (C) are fatal, contradicting this doc's prior "no fatal instance remains" claim.**
This doc previously attributed the family to a frame-slot scan/remap gap in the moving collector's
interpreter frame walk. A full forensic campaign (worktree
`/data/wt-stw-residual-close-20260722`, branch `fix/wildfly-stw-residual-close-20260722`) DISPROVED
that: frames are scanned and remapped correctly in every captured event. The family's true members:

1. **Producer #11 — `Properties.load` re-entrant natives (FIXED, commit `8e1162cfa`).** The fatal
   `MechanismDatabase.<init>` Reader NSME — see
   `docs/internal/fixed-suite-bugs/wildfly-boot-stale-reader-nsme-mechanismdatabase-FIXED.md`.
   0/320 boots post-fix (was the only fatal member).
2. **Frozen-in-JIT peer interpreter-frame coverage (HARDENED, commit `ec883f519`).** A peer frozen
   mid-JIT by the cross-thread STW takeover was covered only by its last root-snapshot deposit +
   the conservative register/native-stack scan; interpreter frames live in Rust Vecs, invisible to
   both. XT-FRAME-SCAN now walks frozen peers' frames on the initiator (JvmThread address published
   with the `tlab_addr` discipline). Rarely exercised in these boots (takeover engages ~never on
   this host's boot profile: `0 newly taken over` across hundreds of passes) but a real gap under
   heavier JIT activity.
3. **OPEN — the non-fatal "StringBuilder chain" shape**, ~3-4%/boot, always healed by the
   all-zero-header CP fallback (boots complete; `WFLYSRV0025` reached; 0 fatal consequences
   observed in 700+ boots). Signature: a chained-append receiver (fresh `StringBuilder`,
   `ArrayList$Itr`, `Optional`) consumed at a `toString`/`getAbsoluteName` append site reads
   all-zero, with the SAME pre-move address propagating the whole chain
   (`ClassToExternalizerMap.toString`, `JndiName.getAbsoluteName`, `Optional.map`).

## What is PROVEN about the open shape (campaign forensics, all tooling landed)

Per-capture facts, consistent across dozens of captures on binaries `fix2`..`fix8`:

- The stale address X **was a key of the fatal epoch's pointer map** (`CRATONVM_DBG_GCPART` ring
  probe: `moved_to=Some(Y)`) — the object was rooted and copied. In one capture the relocated `Y`
  was visible in the SAME frame's healed local while the consumed receiver still read X.
- The holder thread **participated normally** in the fatal collection
  (`CRATONVM_DBG_REMAP_TRACE`: `arrive`/`initiator` entries with the correct map size — in the
  richest capture the holder ITSELF initiated the GC, 71 frames deep at
  `jdk/internal/misc/Unsafe.allocateUninitializedArray0 pc=10` inside real-JDK
  StringBuilder/concat internals invoked from the consuming `toString`).
- The deposit-gap differ (raw frame walk vs deposited snapshot) shows **no exclusion** of X at any
  deposit; the wake-writeback verifiers (`ARRIVE/WAKE/SAFEPOINT-STALE`, `[blockgc]`) are silent;
  `verify_no_stale_refs` runs on the initiator after the remap.
- The popped-slot dump shows X in a **properly tagged object slot** (`raw=0xfffd...`, kind=0) —
  no CompactValue tag/kind anomaly.
- The nret ring shows the append natives returning X repeatedly BEFORE the fatal epoch (the
  pre-GC chain) and — in one capture — **a native returning X one return before consumption,
  post-GC**; the getfield ring shows X was never pushed by an interpreter getfield.
- `CRATONVM_DBG_ZERO_RANGES` places X's memory inside the fatal epoch's own `fromspace-reset`
  wipe (zeroed at collection end — which is also why the funnel's `load_and_forward` return-heal
  cannot recover it afterward).

Net: after a collection that correctly rooted, copied and frame-remapped everything, a pre-move
address re-enters the operand stack through the invoke plumbing — i.e. a Rust-side copy held by
one of the ~70 in-flight interpreter invoke layers (popped-args buffers, return-value plumbing, a
restore path) or a native's internal loop, is pushed after the remap. It is the same CLASS as
producer #11 (raw Rust copies crossing a GC), but the specific holder has not been named yet.

## Queued next step (instrumentation already built and landed)

`push_invoke_return_value` now records every Object invoke-return push in a per-thread ring
(`[pushprov]`, gate `CRATONVM_DBG_REMAP_TRACE`), dumped at every stale-recv/NSME capture next to
the nret ring: a pushprov hit WITHOUT an nret hit = interpreted/lambda/proxy return produced the
stale push; with an nret hit = a native return (the nret site names it). One campaign on a binary
carrying this (first is `cvm-stw-close-20260722-fix9`+) should name the holder directly.
Harness: `probes/batch.sh` (P=4) in the worktree above; pre-fix event rate ~3-4%/boot ⇒ ~10-15
captures per 320-boot campaign.

### Producer #12 — `append(Object)`/`append(CharSequence)` re-entrant toString (FIXED `6e10cba68`)

The bytecode of `ClassToExternalizerMap.toString` settled the sb-chain shape: the poisoned links
are chained returns of **`StringBuilder.append(Ljava/lang/Object;)`** (pcs 110/148: `keys[i]` — a
`Class` — and `values[i]` — an `AdvancedExternalizer`). `native_sb_append_object` captured `this`
raw, ran `invoke_to_string(obj)` — a re-entrant `obj.toString()` (the captures' 71-frame
`Unsafe.allocateUninitializedArray0` dives are `Class.toString()`'s string concat) — then appended
into and RETURNED the raw copy. Same class as producer #11; same fix (pin + `read_native_pin`,
applied to append_object, append_charsequence, the off/len and repeat variants, and
`String.replace(CharSequence,CharSequence)`).

**Verification (campaign `out-run12`, 320 boots):** stale-recv events 2 (0.63%) vs 8-12/320
(2.5-3.8%) one fix earlier; fatal `NSMEDX` 0 — now 960 consecutive fatal-NSME-free boots across
campaigns 10-12. Remaining tail in run12: 2 healed stale-recv + 1 `checkcast PathAddress` CCE-BT +
1 `WFLYCTL0079 org.jboss.as.transactions` rollback-exit ≈ 1.2%/boot — the same long-tail signature
family, almost certainly further members of the SAME re-entrant-native class. Pickup: a static
sweep of every native that calls `invoke_virtual`/`invoke_to_string`/`invoke_interface` after
capturing raw `ObjectRef`s (the `stale-objectref-static-sweep` methodology), or keep flywheeling
captures — each now self-describes via the landed forensics.

### Push-provenance result (2026-07-23, campaign `out-run11`, binary fix9)

The invoke-return ring answered NEGATIVELY: the stale address appears in `[pushprov]` only
~115 invoke-returns BEFORE the fatal epoch (the pre-GC chain, aligned with the `[nret]` wall) —
**nothing re-pushed it post-GC through `push_invoke_return_value`**, and the native-return /
getfield rings are equally silent post-GC, while every dumped LOCAL in every capture is healthy
(so not an `aload` of a stale local either). Remaining un-instrumented channels that could place
the pre-move address into the consuming dispatch:

- non-invoke pushes: `new`-result, `ldc`, `aaload`, `getstatic`, and the kind-preserving
  dup/swap shuffle primitives (`push_with_kind` / `push_compact`);
- any path that resurrects popped-above-`len` slots (the popped-slot dumps prove the pre-move
  bits persist physically above `len`; `update_object_refs` remaps only `0..len` BY DESIGN, so
  any upward `len` restore — deopt/exception/retry machinery — re-exposes unremapped values);
- the in-flight `execute_invoke_kind` `args` buffer if any post-callee code path re-reads it.

Next increment: extend the pushprov recording to the dup-family + `new`/`ldc`/`aaload`/`getstatic`
push sites (or, cheaper, record ONLY pushes whose value lies in the previous epoch's from-space —
a one-comparison gate against the `zero_forensics` newest reset range), then one more campaign.

## Impact assessment for the open shape

Non-fatal in every observation across 700+ instrumented boots: the invokevirtual stale-receiver
detector heals dispatch via the CP class and the boot completes. Residual risks: (a) the healed
dispatch still operates on reclaimed memory (plausible mechanism of the h2
`StringBuilder.append(long)` NaN corruption — writes landing in a zeroed block), and (b) a
consumer without a healing path (checkcast) would make it fatal — no such fatal instance remains
after producer #11's fix.

## Campaign 13/fix11 (2026-07-23): pushprov extended to every interpreter push site — mixed result

Per the "Next increment" above, `push_prov_record` was extended (worktree
`/data/wt-stw-residual-close-20260722`, binary `cvm-stw-close-20260722-fix11.bin`) to fire from
`new`, every `ldc` arm (str/wide-str/classref/condy-cached/condy), the shared `aaload` array-load
arm, both `getstatic` paths (the System.out/err/in intercept and the common
`push_static_field_value` path), and every dup/swap opcode (`dup`, `dup_x1`, `dup_x2`, `dup2`,
`dup2_x1`, `dup2_x2`, `swap`, via a new `record_shuffle_push` helper) — i.e. every Object-producing
push site in the interpreter now feeds the ring, not just invoke-returns.

A fresh 320-boot campaign (`out/results.tsv`, waves 1-80) came back with 3 `STALERECV` events
(0.94%, in line with run12's 1.2%) plus 2 unrelated `EXITED`-only boots and 3 `SLOW` timeouts:

- **2/3 events (boot-147, boot-295) are the SAME long-standing sb-chain shape** (`Optional.map`
  receiver, `[gcpart] moved_to=Some(...)`, `[zeroed] site=fromspace-reset` — the MOVING collector's
  arena reset). `[pushprov]` shows ONLY `invoke-ret` hits (2 and 1 pushes ago) for both — **every
  newly-instrumented channel (new/ldc/aaload/getstatic/dup/swap) is now NEGATIVE for this shape**,
  narrowing the doc's own "remaining un-instrumented channels" list to two candidates: JIT-compiled
  code paths (which bypass `push_prov_record` entirely — the interpreter instrumentation has no JIT
  equivalent) and the upward-`len`-restore / `execute_invoke_kind` args-buffer paths already named
  below. Given `Optional.map`/append chains are hot enough to tier up during a WildFly boot, the JIT
  hypothesis is now the leading candidate — untested as of this writing.

- **1/3 events (boot-134) is a NEW, mechanistically distinct, FATAL bug.** Consumed at
  `DelegatingResource.isRuntime()` → dispatch resolves to an abstract `Resource.isRuntime()Z` with
  no Code attribute → `AbstractMethodError` → `WFLYSRV0056` boot abort. Unlike the sb-chain shape:
  `[gcpart]` shows `moved_to=None appears_as_dest=false` across ALL 5 tracked epochs (this address
  was NEVER part of the moving/copying collector's tracked set), `[zeroed]` shows `site=sweep-span`
  (the NON-MOVING young-gen selective-promotion sweep in `gen_heap.rs`, not the moving arena reset),
  `[pushprov]` shows a `new` hit 48 pushes ago (the object's original, legitimate allocation), and —
  critically — `[getfield] parent=0x20020c94b28 fld[0]` shows the **parent object's live heap field
  CURRENTLY (at capture time) still holds this exact stale pointer**. This is not a raw-Rust-copy-
  escapes-the-remap bug (the producer #11/#12 class); it is a live object's OWN FIELD pointing into
  memory the young-gen sweep already reclaimed and zeroed — i.e. a marking/liveness or
  old-gen→young-gen remembered-set gap. `gen_heap.rs` already has substantial defenses against
  exactly this bug class (`full_old_rset_scan_enabled()` defaults ON — a complete old→young scan
  supplementing the card-table fast path, per the doc comment "a missed write barrier has
  historically manifested as silently emptied Stream/ArrayList results"; `for_each_ref_slot` is
  correctly compact-ref-layout-aware in both the card-scan and full-scan paths), so this is either a
  gap NOT covered by those defenses (e.g. a promotion-timing edge case) or a genuinely new
  regression. **Not yet root-caused.** Next step: `CRATONVM_DBG_RSET_AUDIT=1` (existing, no new code
  needed) was added to `probes/run-one.sh` and a follow-up campaign launched to try to catch this
  shape again with the audit's `[rset-miss]`/`[rset-audit]` output correlatable against a fresh
  stale-recv capture's address — see campaign status appended below once it completes.

- **The run12 `checkcast PathAddress` CCE-BT (boot-157, archived) is ALSO likely part of this
  broader family, but a THIRD distinct shape.** `java.lang.Object cannot be cast to
  org.jboss.as.controller.PathAddress` at
  `UnaryCapabilityNameResolver$1.apply`←`RuntimeCapability.fromBaseCapability`, occurring in the
  middle of a dense burst of `[blockgc] wake tid=N applying K composed fixups` cross-thread
  writeback-heal activity across ~10 threads — i.e. this boot exercised the STW cross-thread
  takeover path (XT-FRAME-SCAN) far more heavily than the ~0-takeover norm this host's boot profile
  usually shows. This lines up with item 2 in this doc's "true members" list above (frozen-in-JIT
  peer interpreter-frame coverage) — "rarely exercised in these boots... but a real gap under
  heavier JIT activity" — this boot is exactly that heavier-activity case. Checkcast has no healing
  path (unlike invokevirtual's CP-class fallback), so this is FATAL. **Not yet root-caused**; needs
  a campaign that captures `CRATONVM_DBG_REMAP_TRACE` alongside `CRATONVM_DBG_CCE_BT` specifically
  during a takeover-heavy boot to get the same forensic depth (`[gcpart]`/`[pushprov]`/`[zeroed]`)
  the stale-recv path already has for the other two shapes — the CCE-BT capture currently only
  dumps the Java call stack, not the GC forensics.

- **The run12 `WFLYCTL0079` transactions-module rollback-exit (boot-186, archived) is a SEPARATE,
  FOURTH shape, likely unrelated to the GC stale-ref family.** Root cause:
  `WFLYCTL0043: An attribute named 'hornetq-store-enable-async-io' is already registered at
  location '/subsystem=transactions'` — a genuine DUPLICATE attribute registration, i.e. some
  extension-initialization code path ran twice. This smells like a class/loader-identity duplication
  bug (the same family as `docs/internal/aot-beanoverride-double-context-refresh-rootcaused-*`'s
  fork-loader ClassId instability) rather than a stale-pointer read. **Not yet root-caused; not
  confirmed related to this doc's family** — flagged here only because it was in the same
  1.2%-tail sample as the other three.

- **Two unrelated `EXITED`-only boots (boot-47: `IllegalArgumentException: No enum constant
  MINUTES`; boot-144: `IllegalArgumentException: No enum constant LOCAL_USE_7`)** carry no
  `[stale-recv]`/`[pushprov]`/`[zeroed]` forensics at all — almost certainly pre-existing,
  unrelated environment/config flakiness (a real enum constant like `MINUTES` failing
  `valueOf()` smells like a split-classloader/duplicate-enum-class issue, a different bug
  entirely). Not investigated further; out of scope for this doc.

**Net: what was one "OPEN" line item is now four,** three of them genuinely new discoveries this
instrumentation surfaced rather than resolutions of the original shape. The original sb-chain shape
survives a now-exhaustive interpreter-side pushprov sweep and points at JIT-compiled code as the
next and likely final interpreter-adjacent hypothesis to test.

## RSET_AUDIT diagnostic hardening + negative reproduction result (2026-07-23)

To test the boot-134 mark-sweep-liveness hypothesis above, `CRATONVM_DBG_RSET_AUDIT` (a pre-existing
but apparently never-exercised-under-WildFly diagnostic — a full old-gen→young-gen scan that flags
"clean card on a live edge" write-barrier misses) was enabled in `probes/run-one.sh`. Turning it on
immediately found — and this campaign then fixed — **an unrelated, genuine bug in the diagnostic
itself**, independent of everything else in this doc:

- `CRATONVM_DBG_RSET_AUDIT`'s old-gen field walk and a companion young-gen "[small4]" linear scan
  both hand-derived field offsets assuming the legacy 16-byte `Value`-tagged cell layout
  unconditionally — never updated for the now-default-on compact-ref-field layout. Enabling the
  flag SIGSEGV'd **~17% of boots** in the first campaign that exercised it. Fixed the old-gen walk
  by switching to the existing unified `for_each_ref_slot` helper (same one `scan_all_old_to_young`
  already used correctly) — `gc/src/gen_heap.rs`.
- That still left the young-gen "[small4]" scan crashing (~6% of boots, second campaign): a
  DIFFERENT, orthogonal bug — that walk has no free-list/TLAB-gap-aware cursor advancement (unlike
  the `young_object_starts` walk elsewhere in the same function, hardened 2026-07-16 for the
  cce0079 desync), so landing on a stale TLAB-tail byte pattern that happens to parse as a
  plausible-but-wrong header desyncs it for the rest of the arena — observed as `num_slots` in the
  hundreds of thousands and a walk into unmapped memory. A `num_slots` plausibility cap closed the
  Object-kind crash shape but a THIRD campaign still found crashes (down to baseline ~0.6%, but one
  of two `EXITED` boots was still this walker). Rather than keep chasing an inherently unsound linear
  walk with narrower caps, it was split into its own separately-gated flag
  (`CRATONVM_DBG_RSET_AUDIT_YOUNG_SCAN`, NOT set by the harness) so `RSET_AUDIT`'s actually
  load-bearing old→young scan — now fixed and verified clean across 3 smoke-test boots — is usable
  without inheriting the young-walk's fragility. **Net: three real (if minor, debug-only, opt-in)
  bugs found and fixed as a side effect of trying to instrument this investigation further.**

**The hypothesis itself remains UNTESTED.** Across four more 320-boot campaigns run while chasing
the above (~1250 additional boots total), the boot-134 `site=sweep-span` shape did **not**
reproduce again — every other `STALERECV` capture in this stretch (boot-77, -234, -238, -275, plus
the original -147/-295) was the ordinary `site=fromspace-reset` sb-chain shape. Boot-134 remains a
single occurrence; whatever produces it is rarer than roughly 1-in-1500 boots, or specific to some
timing/config window not hit again in this stretch. The `[rset-miss]`/`[rset-audit]` correlation
this diagnostic was built to provide is still unexercised for this specific bug — it needs either a
much larger campaign or a lucky repro to actually test the missed-write-barrier hypothesis.

## Session status summary (2026-07-23, end of session)

What is FIXED and verified this session:
- pushprov ring extended to every interpreter Object-push site (`new`, every `ldc` arm, `aaload`,
  both `getstatic` paths, and the full dup/swap family) — real, working instrumentation, committed.
- `CRATONVM_DBG_RSET_AUDIT`'s two compact-ref-layout field-walk bugs — real SIGSEGV fixes,
  independent of the main investigation, verified via 3 clean campaigns + smoke tests.

What is OPEN, in priority order:
1. **Original sb-chain shape** (`Optional.map`/StringBuilder-chain, `fromspace-reset`,
   `invoke-ret`-only pushprov) — now pushprov-exhausted for every interpreter channel. Leading
   hypothesis: JIT-compiled code paths (untested — no JIT-side pushprov equivalent exists).
2. **boot-134 mark-sweep liveness gap** (FATAL, `AbstractMethodError`) — mechanistically
   characterized (parent field stale, non-moving sweep zeroing, never in moving-GC epochs) but not
   reproduced again to test the missed-write-barrier hypothesis; needs a much larger campaign.
3. **checkcast `PathAddress` CCE-BT** (FATAL, `ClassCastException`) — correlated with heavy
   cross-thread STW-takeover activity (dense `[blockgc] wake ... composed fixups` burst); likely
   the "frozen-in-JIT peer interpreter-frame coverage" gap this doc already names as "rarely
   exercised... but a real gap under heavier JIT activity" — not yet captured with GC forensics
   (only has a Java-stack dump, needs `REMAP_TRACE`+`GCPART` alongside `CCE_BT` on a takeover-heavy
   boot).
4. **`WFLYCTL0079` duplicate attribute registration** (FATAL, `WFLYCTL0043` dup-register) — likely
   unrelated to the GC family entirely; a class/loader-identity duplication smell, not investigated.

None of items 1-4 are fixed. This doc should stay OPEN with this characterization until a future
session reproduces and root-causes at least the two fatal items (2, 3).

## WFLYCTL0079 double-dispatch hypothesis test (2026-07-23) — inconclusive, diagnostic landed

Added `CRATONVM_DBG_DUPCALL_FILTER` (`push_frame_and_fire_entry`'s chokepoint in interpreter.rs,
gated by a new `env_cache::dbg_dupcall_filter`): traces every entry to
`ParallelExtensionAddHandler$ExtensionInitializeTask.call()` with the receiver's identity and
thread id, to test whether item 4 (`WFLYCTL0079`) is caused by the boot executor double-dispatching
the same task (each task's `call()` should legitimately run exactly once per extension per boot —
decompiling the class confirmed exactly one `executor.submit()` per extension, so a genuine second
execution would be a real CratonVM executor/queue correctness bug).

**False-positive found and fixed first:** the task class implements `Callable<V>`, so it has a
compiler-generated bridge method (`call()Ljava/lang/Object;`) alongside the real covariant-return
method (`call()Lorg/jboss/.../OperationFailedRuntimeException;`) — both named `call`, so every
logical invocation legitimately produces TWO frame-push events (bridge → real), on the SAME thread,
back-to-back, on **every single task, every single boot**. Confirmed via the first smoke-test boot
(72 `[DUPCALL]` lines for 36 tasks, every receiver appearing in a consecutive same-thread pair).
`probes/run-one.sh`'s tag detection was corrected to flag only a receiver seen **3+** times
(`DUPCALL3X`) as a genuine extra invocation — 2 is the expected bridge-pair baseline.

**Result: inconclusive.** Two independent 800-boot campaigns (1600 boots total) with the corrected
diagnostic active caught neither a `DUPCALL3X` nor a fresh `WFLYCTL0079`/`DUPATTR` occurrence. Given
the bug's own historical rate (~2 occurrences in ~2400 boots run without the diagnostic, i.e.
roughly 1-in-1200), going 1600 boots without a repeat is unsurprising sampling variance, not
evidence against the double-dispatch hypothesis. The diagnostic itself is safe (zero false positives
across 5 smoke tests + 1600 campaign boots) and committed to dev — a future session can either keep
sampling with `CRATONVM_DBG_DUPCALL_FILTER=1` already wired into `probes/run-one.sh`, or pursue the
class-init-twice alternative hypothesis instead.

## WFLYCTL0079 round 2 + CCE-BT forensics extension (2026-07-23)

Added a SECOND, more precise diagnostic (`DUPREG`, tracing
`TransactionSubsystemRootResourceDefinition.registerAttributes()` directly — the actual
registry-mutation call site, several frames below the executor task dispatch `DUPCALL` already
tested clean) after decompiling the class confirmed `HORNETQ_STORE_ENABLE_ASYNC_IO` is registered
via an `AliasedHandler` inside this one instance method. Unlike `DUPCALL`, this method has no
bridge-method ambiguity — any repeat of the same (receiver, registration-registry) identity pair is
unambiguously a genuine double call. An 800-boot campaign (`DUPREG2X`/`DUPATTR` tags) came up empty
— survived two silent mid-campaign deaths from host contention (50+ concurrent users; `setsid`
detachment fixed it) before finally completing clean.

That campaign's real find was a **second checkcast-CCE occurrence** (target=
`com.squareup.protoparser.FieldElement$Label`, NOT `PathAddress` — confirms the family isn't
specific to one target class, it's a general "any checkcast can hit a degraded-to-bare-Object
receiver" gap), non-fatal this time (WildFly's MSC marked the owning service FAILED and continued
booting), with 246 `[blockgc] wake` cross-thread writeback events in the same boot — supporting the
"correlates with heavy STW cross-thread takeover activity" hypothesis with a second data point.
`CRATONVM_DBG_CCE_BT` had never captured GC forensics before (only a Java call-stack dump), so it
was extended to reuse the exact same `gcpart`/`pushprov`/`zeroed`/`getfield` probes `[stale-recv]`
already has, gated on the CCE'd receiver having degraded to a bare `java.lang.Object` (this family's
signature — an app-level type-mismatch CCE has nothing useful to probe). A THIRD 800-boot campaign
with this extension active came up empty for `CCEBT` too (checkcast-CCE is itself rare, ~2
occurrences across ~3200+ boots this session).

**Running total across all three campaigns with active double-invocation tracing: 2400 boots, 0
`DUPCALL3X`, 0 `DUPREG2X`, 0 fresh `WFLYCTL0079`.** Combined with the original ~2400 boots (2
occurrences, no tracing), the true base rate is likely rarer than the earlier ~1-in-1200 estimate —
that estimate was from n=2 and is noisy. Both double-dispatch hypotheses (executor-level and
registry-mutation-level) are now reasonably well tested and NOT confirmed; if `WFLYCTL0079` is
still a double-execution bug, it must be more localized than either level traced so far (e.g. inside
`AliasedHandler`'s own construction), or it is a different mechanism entirely (a genuine WildFly-side
non-determinism, possibly config-order or hash-iteration-order dependent, unrelated to CratonVM
double-dispatch). **Recommendation for continuation:** further blind campaigns have poor
cost/reproduction odds at this rate; a more targeted move would be tracing `AliasedHandler`'s own
constructor/registration call directly (same technique, one level deeper), or giving up on live
reproduction and instead statically auditing `AliasedHandler`'s WildFly-side registration logic for
a genuine non-atomic check-then-act pattern.

## Large-scale campaign (800 boots, fix15, 2026-07-23) — WFLYCTL0079 confirmed reproducible

With the RSET_AUDIT diagnostic now fixed (verified holding at scale: 1 `EXITED`/800, no segfaults),
an 800-boot campaign was run to try to catch item 2 or 3 again. Results: 7 `STALERECV` (0.875%,
consistent with prior rates), 1 `EXITED` (0.125%, near baseline), 0 `CCEBT`, 0 `NSMEDX`.

- **6/7 stale-recv events are the ordinary sb-chain shape** (`fromspace-reset`, `invoke-ret`-only
  pushprov) — no new data.
- **1/7 (boot-760) superficially LOOKED like item 2's shape** (`[zeroed]` lists a `site=sweep-span`
  entry) but on inspection it is a coincidence, not a repeat: the sweep-span match is a stale
  `age=3036` ring entry from address reuse (a prior, unrelated object swept from this same address
  long ago), while `[gcpart]` shows `moved_to=Some(...)` at epoch 4 (unlike boot-134's `None` across
  every epoch) and two much fresher `fromspace-reset` zero records dominate — this is the ordinary
  moving-collector sb-chain shape, not item 2. **Item 2 (boot-134's exact shape) still has not
  reproduced** across ~2450 boots run this session; it remains a single occurrence.
- **The 1 `EXITED` boot (boot-739) is item 4 (`WFLYCTL0079`/`WFLYCTL0043` duplicate
  `hornetq-store-enable-async-io` attribute registration) reproducing with the IDENTICAL signature
  as the original run12 occurrence (boot-186).** Two occurrences, same exact error string, both
  during `ParallelExtensionAddHandler`/`parallel-extension-add` — this is now a confirmed-
  reproducible bug (~1-2 per 1600 boots), not a one-off. It has no `[stale-recv]`/`[pushprov]`
  forensics (this doc's instrumentation doesn't cover it) and may not even be a CratonVM bug — it
  could be a genuine WildFly-level race in the transactions-extension attribute builder that
  HotSpot's different scheduling/timing rarely exposes. The one CratonVM-side hypothesis worth
  checking first: whether `<clinit>` (class static initialization, JLS-guaranteed to run exactly
  once) can run twice under CratonVM's class-loading lock during heavy concurrent
  `parallel-extension-add` — that would be a fundamental correctness bug, not specific to this one
  attribute, and the natural next diagnostic step (a per-class-init entry/exit trace under
  contention) for whichever future session picks this up.

## Historical characterization (2026-07-22, superseded in mechanism, preserved)

The discriminating capture that opened this doc (fix6 campaign, `CRATONVM_DBG_STALE_RECV=1`)
showed the stale receiver in NO local of any frame, consumed at two consecutive append pcs, with
every native producer of the remoting-cce family fixed on that binary. The conclusion drawn then —
"a reference sitting on an INTERPRETER OPERAND STACK across a nested allocating call can read back
stale — the frame's stack slot missed the moving-GC root scan/remap" — was WRONG in mechanism:
"locals healthy" was survivorship (locals mostly reference older objects), and the stack slot was
never missed by the remap; the stale value re-entered via Rust-side invoke plumbing after the
remap. The suggested pickup (audit the frame stack scanner's slot classification) was carried out
during this investigation and found sound.
