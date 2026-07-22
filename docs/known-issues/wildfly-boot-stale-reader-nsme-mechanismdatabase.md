# WildFly standalone boot: `NoSuchMethodError: java/lang/Object.read([CII)I` in Elytron `MechanismDatabase.<init>` — stale Reader ref, one sighting

Status: OPEN — observed once (2026-07-22, fix3 verification campaign attempt `boot-96`,
worktree `/data/wt-remoting-cce-close-20260722`, binary `cvm-remoting-cce-close-20260722-fix3.bin`),
fatal (`parallel-extension-add` rollback → `System.exit(1)`).

## Symptom

```
WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError method="java/lang/Object.read([CII)I"
     caller="org/wildfly/security/ssl/MechanismDatabase.<init>(Ljava/lang/String;Z)V @pc=47"
[cratonvm] System.exit(1) called — process terminating
```

A `Reader.read(char[], int, int)` dispatch whose receiver resolved to bare `java.lang.Object` — the
stale-ObjectRef family's cid=0 signature (zeroed/reused block) surfacing at INVOKE dispatch rather
than at a checkcast. Elytron's `MechanismDatabase` constructor reads its bundled
`MechanismDatabase.properties` through an `InputStreamReader`/`BufferedReader` chain during the SSL
subsystem's `parallel-extension-add` initialization; the reader ref it dispatched on was a pre-move
address.

## Distinctness

This is NOT one of the producers closed by `fix/wildfly-remoting-cce-close-20260722`
(Properties entrySet materialization, `collect_entries_via_iterator`, map-copy constructors,
`Map.getOrDefault` raw defaults — see
`docs/known-issues/wildfly-remoting-classcastexception-parallel-extension-add.md`). The consumer is
an io Reader, not a collection view; the producing native is presumably in the
`native-io` reader/decoder construction or read path (the historical `alloc_stream_decoder` family —
`docs/internal/wildfly-parallel-boot-stale-objectref-residual.md` — was hardened in 2026-07-11's
sweep, so this is either a missed site or a different io-side holder).

## Capture tooling (already landed)

Commit `9dc757731` (same branch) extends `CRATONVM_DBG_CCE_BT` to dump the Java frame stack
(`CCE-BT-STK[n]` lines) whenever a dispatch miss's receiver resolved to bare `java/lang/Object`
(`site=nsme_dispatch`) — the exact instrumentation that named the `getOrDefault` producer within one
campaign wave. The one sighting predates the tracer build, so no stack exists yet.

## Second sighting WITH stack (2026-07-22, fix6 campaign boot-139)

The `site=nsme_dispatch` tracer captured the full frame chain:
`MechanismDatabase.<init> pc=47` ← `<clinit>` ← `CipherSuiteSelector.fromNamesString` ←
`SSLDefinitions$CipherSuiteFilterValidator.validateParameter` ← `AttributeDefinition.validateAndSet`
← `AbstractAddStepHandler` ← `ParallelBootOperationStepHandler$ParallelBootTask.run` — i.e. the
elytron cipher-suite validation running MechanismDatabase's properties-read loop during
parallel-extension-add. The reader ref dies inside the constructor's own read loop. Reader
CONSTRUCTION natives (`alloc_stream_decoder` family) were pin-hardened in the 2026-07-11 sweep, and
on this binary every collection/getOrDefault/ResourceBundle/StringBuilder producer is fixed — this
sighting therefore belongs to the interpreter frame-slot staleness characterized in
`docs/known-issues/interpreter-operand-stack-slot-stale-after-nested-alloc.md` (same pickup: the
moving collector's interpreter frame scan), not to a native producer. Rate: 2 sightings in ~1,500
combined post-fix attempts.

## Next steps

- Re-run the `xargs -P 10` isolated `standalone.sh` harness
  (`/data/wt-remoting-cce-close-20260722/probes/`, `CRATONVM_DBG_CCE_BT=1`) on a tracer-carrying
  binary until `site=nsme_dispatch` fires with `MechanismDatabase` frames; the stack will name the
  producing frame directly.
- Static candidates to audit while waiting: the `native-io` `InputStreamReader`/`BufferedReader`
  construction natives and any Rust-side holder of reader refs across GC-capable calls in the
  `getResourceAsStream` → reader chain (same "raw local across allocation" shape as every other
  member of this family).
- Rate: 1 in ~710 combined post-fix attempts on 2026-07-22 (~0.1-0.15%/attempt) — noticeably rarer
  than the closed producers (~0.5-0.7% each).
