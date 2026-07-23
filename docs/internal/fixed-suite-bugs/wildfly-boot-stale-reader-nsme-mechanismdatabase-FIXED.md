# WildFly standalone boot: `NoSuchMethodError: java/lang/Object.read([CII)I` in Elytron `MechanismDatabase.<init>` — stale Reader ref — FIXED

Status: **FIXED 2026-07-23** — producer #11 of the parallel-extension-add stale-ref family
(branch `fix/wildfly-stw-residual-close-20260722`, commit `8e1162cfa`).
Verified: **0 occurrences in 320 isolated boots** on the fixed binary (campaign `out-run10`,
worktree `/data/wt-stw-residual-close-20260722/probes/`) vs 2/81 + 1-2 per ~100 boots on the same
harness pre-fix — and the historical 2/~1500 on the (slower-timing) 2026-07-22 remoting-cce
binaries.

## Root cause (captured live, not inferred)

`native_properties_load_reader` — the CratonVM native for `Properties.load(Reader)` — pulls the
Reader's contents via a loop of **re-entrant** `ctx.invoke_virtual(reader, "read", "([CII)I", ...)`
calls while holding `this`/`reader`/the scratch `char[]` as **raw Rust `ObjectRef` copies**.
Elytron's `MechanismDatabase.<init>` reads `MechanismDatabase.properties` through exactly this
native (the reported "caller `MechanismDatabase.<init> pc=47`" is the nearest Java frame — the
native creates none). The nested dispatch runs 7+ frames of real-JDK bytecode
(`InputStreamReader.read` → `StreamDecoder` → ... → `Unsafe.allocateUninitializedArray0`), which
hits interpreter safepoints and allocates — so during `parallel-extension-add`'s allocation storm a
**moving young collection** frequently runs inside one of the loop's nested reads (either
initiated by this very thread's allocation, or arrived-at cooperatively).

The collection remaps every *tracked* location (frames, pins, snapshots) — but not the native's raw
Rust locals. Critically, the moving young collector's `Arena::reset` **zeroes the evacuated
from-space at collection end**, so the funnel's return-value `load_and_forward` heal cannot recover
the raw copies afterward (the forwarding metadata is gone; the memory reads all-zero). The next
loop iteration dispatches `read` on the stale `reader` → the receiver resolves as bare
`java/lang/Object` → `NoSuchMethodError: java/lang/Object.read([CII)I` → `parallel-extension-add`
rollback → `System.exit(1)`. The stale `char[]` also silently yields garbage chars — a data
corruption in the same window.

`drain_input_stream`'s strategy-3 loop (`Properties.load(InputStream)` and other callers via
`drain_input_stream_pub`) had the identical shape, and both `load` overloads also used the raw
`this` after the drain for the side-table store.

The decisive capture chain (all tooling landed, env-gated): `CRATONVM_DBG_REMAP_TRACE`'s
participation trace showed the holder **arriving at the fatal epoch inside
`java/io/InputStreamReader.read pc=10`, 20 frames deep** — i.e., inside the re-entrant read;
`CRATONVM_DBG_GCPART` showed the reader's address as a key of that epoch's pointer map (rooted and
copied — via the nested callee's frames — while the native's copies stayed stale);
`CRATONVM_DBG_ZERO_RANGES` placed the address inside the `fromspace-reset` wipe range.

## Fix

Commit `8e1162cfa`: pin `this`/`reader`/`stream`/scratch buffer via `pin_native_root` at entry and
re-read them via `read_native_pin` after **every** re-entrant call (pins are remapped in place by
all three GC paths: initiator, safepoint-arrival, blocked-wake) — the same discipline as
remoting-cce producers 1-10. Also refresh `this` before `store_parsed_entries` in both `load`
overloads.

**The recurring lesson (now 11 producers deep): any native that re-enters Java while holding a raw
`ObjectRef` across the call is wrong.** The funnel pins its *arguments*, but everything a native
derives or holds privately must go through the pin table if any nested `invoke_*` /
blocking-region call follows. `load_and_forward` after the fact is NOT a substitute — from-space
forwarding dies at collection end under the generational moving young path.

## Historical filings (preserved)

Original filing 2026-07-22 (one sighting, fix3 campaign boot-96) and second sighting with the
`site=nsme_dispatch` frame chain (fix6 boot-139):
`MechanismDatabase.<init> pc=47` ← `<clinit>` ← `CipherSuiteSelector.fromNamesString` ←
`SSLDefinitions$CipherSuiteFilterValidator.validateParameter` ← `AttributeDefinition.validateAndSet`
← `AbstractAddStepHandler` ← `ParallelBootOperationStepHandler$ParallelBootTask.run`.
The doc's earlier hypothesis ("belongs to the interpreter frame-slot staleness … the moving
collector's interpreter frame scan") was wrong in the specific mechanism — the interpreter frames
were remapped correctly; the stale copies lived in the `Properties.load` native's Rust locals.
