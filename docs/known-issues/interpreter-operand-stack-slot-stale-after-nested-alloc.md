# Stale refs surfacing on interpreter operand stacks after a nested allocating call — the residual core of the WildFly cid=0 family

Status: **RE-CHARACTERIZED 2026-07-23; fatal member FIXED; one non-fatal shape remains OPEN.**
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

## Impact assessment for the open shape

Non-fatal in every observation across 700+ instrumented boots: the invokevirtual stale-receiver
detector heals dispatch via the CP class and the boot completes. Residual risks: (a) the healed
dispatch still operates on reclaimed memory (plausible mechanism of the h2
`StringBuilder.append(long)` NaN corruption — writes landing in a zeroed block), and (b) a
consumer without a healing path (checkcast) would make it fatal — no such fatal instance remains
after producer #11's fix.

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
