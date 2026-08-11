# `net_phase_e.rs` under strict: one door defect, two behaviour carriers, and what "fix the door" is not

**Status: source changes APPLIED, NOT REBUILT, 2026-08-11.** Every number below
was taken by running the already-built `dev` binary at
`C:/craton/CratonVM/target/release/cratonvm.exe` (built 2026-08-11 19:41), the
JDK 25 image at `C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot`, and
that image's `javap` and `java`. **No claim is made that the source changes in
this branch compile or work** — the binary predates them, and predates several
fixes merged to `dev` today.

Branch: `fix/jdk-only-httpserverloop-and-strict-fallbacks-20260811`.
Files changed: `native-builtins/src/net_phase_e.rs` and this record.

This record continues `W7-17-vm-internal-door-sweep.md`, which classified 44
VM-minted classes three ways and left one open fatal door defect in this file.
It applies that fix (§1), and then does what the sweep says the *other* ~35
classes need: it takes this file's own surface, finds every site that mints a
compatibility stand-in whose natives strict drops, and puts a **real-JDK
fallback at the point of refusal** (§3).

---

## 1. `CratonVM$HttpServerLoop` — the door defect, and both gates checked

`com.sun.net.httpserver.HttpServer.start()` could not start under `--jdk-only`.
Reproduced on the shipped binary before touching anything:

```
$ cratonvm --jdk-only --java-home <JDK25> -cp <probes> HttpServerWildcardAddressProbe
channel getLocalAddress()   = /0.0.0.0:63877
channel SO_REUSEADDR        = true
Exception in thread "main" java/lang/NoClassDefFoundError: CratonVM$HttpServerLoop
	at HttpServerWildcardAddressProbe.main(HttpServerWildcardAddressProbe.java:49)
```

The same probe under `--real-jdk` prints all four address lines and exits Ok, so
this is strict-only. `bind()` and the wildcard-address reporting above it are
unaffected; `start()` is the call the door takes out, because
`re10_spawn_dispatcher` is reached from the `HttpServer.start` native and
`?`-propagates the refusal out of it.

**Was the recorded patch still needed?** Yes. `W7-17` §6 hunk A carries the
code; this campaign has fifteen records claiming a patch was never applied when
it was already in the tree, so it was checked against the source and against
`dev` first, not against the record: `git show origin/dev:native-builtins/src/net_phase_e.rs`
contains **zero** occurrences of `ensure_vm_internal_class`, and the mint site
was the bare `try_alloc_concurrent_synthetic(ctx, HS_LOOP_CLASS, 1)?`.

**Gate 1 — the class.** `javap CratonVM.HttpServerLoop` against the JDK 25
image answers *class not found*; the name is not in a JDK namespace at all. It
is a 1-slot `Runnable` this VM invents to carry `server_id` from
`re10_spawn_dispatcher` to `re10_serve_loop_run`, minted four times (one per
`HS_DISPATCHER_POOL` dispatcher) — contract §1 item 6's shape, permitted in
every mode through `ensure_vm_internal_class`. Through
`try_alloc_concurrent_synthetic` alone it took `ClassOrigin::CompatibilityStub`,
which `--jdk-only` correctly refuses.

**Gate 2 — its natives. Measured per class and per mode, not by analogy.**
`--dump-native-registry` over a boot in each mode reports exactly one
registration under this class name, and the same kind in both:

```json
compatible: {"class":"CratonVM$HttpServerLoop","name":"run","descriptor":"()V",
             "kind":"bridge","registered_by":"native-builtins/src/net_phase_e.rs:16435",
             "kind_stated":false,"kind_chosen":true,"owns_slot":true}
jdk-only:   {"class":"CratonVM$HttpServerLoop","name":"run","descriptor":"()V",
             "kind":"bridge", … identical … }
```

(The dumps' own `counts` line is the control that the strict dump really is
strict: `synthetic-stub` 1,262 in Compatible, **0** under `--jdk-only`, with
`bridge` 9,740 in both.) Read against `native-api/src/no_image_receiver.rs`,
that is what the tables predict: the name does not start with `cratonvm/`, so
`receiver_declared_by_no_supported_image` never consults
`VM_MINTED_STAND_IN_RECEIVERS`; it is in neither `NO_IMAGE_JDK_RECEIVERS` nor
`VM_SERVICE_RECEIVERS` nor `STRICT_STILL_FABRICATES`; so nothing re-tags it
`SyntheticStub` and the `JdkOnly` drop arm never sees it.

**Gate 2 is already open, gate 1 alone was refusing, so the door fix is
necessary *and* sufficient here** — which is not true of most of `W7-17`'s
table, and is the whole reason that record insists the registry dump is taken
twice. A concurrent lane measured the counter-example on
`cratonvm/internal/LinkedListSnapshotListItr`: clearing gate 1 alone moved the
failure from `NoClassDefFoundError` at the mint to `UnsatisfiedLinkError` at the
first call, which is a moved symptom, not a fix.

The fix is the one-line pre-mint from `W7-17` §6 hunk A, with its reasoning
written onto the site. It is a pre-mint rather than a replacement because
`fabricate_class` returns the existing `ClassId` for an already-loaded name
before it reaches `admit_compatibility_class`, so the allocation below keeps its
real-vs-requested field-count widening and its GC-safe retry unchanged, and only
the recorded ORIGIN moves. The `try_alloc_concurrent_synthetic(ctx,
"java/lang/Thread", …)` two lines below is deliberately untouched:
`java/lang/Thread` has real bytes, `fabricate_class` loads them, and no
`compatibility-class-requested` row is ever emitted for it.

### `Compatible` mode, at this site

**Unchanged.** `Compatible` never refuses the mint, so the only thing that moves
is the class's recorded origin: `--dump-class-origins` reports `vm-internal`
instead of `compatibility-stub`, `--jdk-only-report`'s
`counts.compatibility_classes` drops by one and `counts.generated_classes` rises
by one, and `vm_exec.rs`'s `stub_hint` stops appending *"class not found on any
classpath entry — synthetic stub, add the missing jar"* to a `NoSuchMethodError`
naming a class no jar can ever contain. All three are corrections.
`BASELINE_SYNTHETIC_STUBS` / `stub_ratchet` do not move: they count
`SyntheticStub`-tagged **registrations**, and this class's one registration is a
`bridge` in both modes (above), so there is nothing for them to count.
