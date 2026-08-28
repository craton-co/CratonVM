# The definition-of-done screen, run for the first time — 0 fabricated classes instantiated, 4 requests, all named

**Status: MEASURED 2026-08-28.** Binary from `f55556e41`. No fix in this page;
it is the instrument reading, and one OPEN item it sharpens.

## 1. What the roadmap actually asks for

`docs/feature-designs/jdk-only-completion-roadmap.md` §6:

> **Definition of done.** Not a suite number. A Spring Boot application, a
> servlet container serving HTTPS, and a JDBC workload each run to completion
> under `--jdk-only` with **no fabricated class instantiated, whatever its
> package** — screened against the refused-class set the VM reports, not against
> a prefix.

The prefix clause is load-bearing: six of the nine fabricated classes the
roadmap names do **not** match `cratonvm/internal/`, so a prefix screen would
have reported Phase 1 clean with every one of them still fabricated.

The instrument is `--jdk-only --explain-jdk-only --jdk-only-report <file>`, and
this page is the first time it has been pointed at the definition of done rather
than at a census.

**None of the three named workloads is checked out on this host** — only their
runners are (`apps/spring-boot/`, `apps/h2database-suite-runner/`). So this is
the screen applied at the scale available, and it is NOT the definition of done
being met. It is the same instrument on smaller programs, and it says something
useful anyway.

## 2. The reading

Five probes, each `--jdk-only`, each a program that exercises a named family:

| probe | `compatibility_classes` | fabrication requests | `native-shadows-bytecode` | `synthetic_stub_invocations` |
| --- | ---: | ---: | ---: | ---: |
| `Phase1Sweep` | **0** | 2 | 202 | **0** |
| `Phase3Sweep` | **0** | 1 | 200 | **0** |
| `KeyStoreFamilySweep` | **0** | 1 | 161 | **0** |
| `InetFamilySweep` | **0** | 0 | 88 | **0** |
| `AtomicUpdaterSweep` | **0** | 0 | 58 | **0** |

Two columns matter most, and they are the two the roadmap put at the centre:

* **`compatibility_classes: 0` everywhere.** Not one fabricated class was
  *instantiated* on any of the five paths. That is the definition-of-done
  predicate, and on these programs it holds.
* **`synthetic_stub_invocations: 0` everywhere.** Strict mode drops every
  `SyntheticStub` and nothing on these paths breaks — which is the premise
  Phase 2 rests on, measured rather than assumed.

## 3. The four requests, each with its requester line

A **request is not a failure**, and this is the rule that makes the census
usable: the native asks, is correctly refused, and the caller recovers onto real
JDK bytecode. That is strict mode working as designed. **The blocking set is the
intersection: refused *and* not recovered from.**

```text
java/util/Enumeration$Impl                   native-builtins/src/classloader.rs:6299
java/util/IteratorEnumeration                native-builtins/src/keystore.rs:3106
cratonvm/internal/SystemLogger               native-builtins/src/lib.rs:27767
cratonvm/internal/foreign/MemorySegmentImpl  native-builtins/src/panama.rs:191
```

**Three of the four recovered**, and the probe rows prove it rather than
asserting it: `Phase3Sweep` is 35/35 clean with the `Enumeration$Impl` request
in its report; `KeyStoreFamilySweep`'s only residual is the JCEKS gap, not
anything enumeration-shaped; `Phase1Sweep` produced all 80 rows with no
logging-shaped difference.

## 4. The one that does NOT recover cleanly, now with a line number

`cratonvm/internal/foreign/MemorySegmentImpl`, requested at `panama.rs:191`.

When `craton_segment_class_id` is refused, `alloc_segment_carrier` falls back to

```rust
return try_alloc_concurrent_synthetic(ctx, PE_SEGMENT_INTERFACE, slots);
```

— allocating an object whose class is **`java/lang/foreign/MemorySegment`, the
INTERFACE**. Measured (`probes/Phase1Sweep.java`, P1-E):

```text
Arena.ofConfined().allocate(16).getClass().getName()
  HotSpot   jdk.internal.foreign.NativeMemorySegmentImpl
  CratonVM  java.lang.foreign.MemorySegment          <- an interface, as an instance's class
```

`byteSize()` still answers 16, so this is identity rather than function — but an
instance whose class is an interface is a thing the Java object model does not
contain, and the usual next moves (`getClass().getSuperclass()`,
`isInstance`, a class-keyed cache, any serializer) are entitled to assume it
cannot happen.

This is the same shape as the recorded
`fabricating-an-abstract-class-trades-an-npe-for-an-abstractmethoderror`: a
refusal fallback that hands back an abstract carrier moves the failure one frame
on rather than removing it.

**Not fixed here, deliberately.** The fallback cannot simply name the real
`jdk.internal.foreign.NativeMemorySegmentImpl`: that class has its own field
layout and this module's natives address raw slots, so adopting it is a layout
question, not a rename. It is `panama.rs`'s lane and its own piece of work.

## 5. Phase 2's target is HALF what the row count says — the report already knew

`native-shadows-bytecode` is Phase 2's target measured on the path a real
program takes. Union over the five probes: **805 rows**. But the rows carry an
`outcome` field, and nothing has been reading it:

```text
outcome        native-won    579 rows    334 DISTINCT triples
               bytecode-won  226 rows    104 DISTINCT triples
native_kind    bridge-ran-over-bytecode  579
               bridge                    206
               check-override-name        20
```

**A `bytecode-won` row is not a shadow that needs retiring — it is one that has
already lost.** The native is registered, the dispatch preferred the real JDK
bytecode anyway, and the row records the near-miss. Counting those into the
retirement worklist inflates it by about 40% and points effort at triples where
there is nothing to remove.

So on these paths Phase 2's actionable surface is **334 distinct triples**, not
805 rows and not 438 triples. Top of it:

```text
39  java/lang/Class                    16  sun/security/pkcs12/..$DualFormatPKCS12
35  jdk/internal/misc/Unsafe           16  sun/security/provider/..$DualFormatJKS
30  java/lang/StringBuilder            14  java/util/HashMap$KeyIterator
30  java/util/Arrays                   14  java/util/concurrent/CopyOnWriteArrayList
30  java/util/HashSet                  14  java/lang/System
29  java/util/concurrent/ConcurrentHashMap  13  jdk/internal/access/SharedSecrets
19  java/util/HashMap                  11  java/lang/Module
```

This is the roadmap's own §2 finding one level down. That section says the
report "is meant to be consumed and nothing was consuming it"; a consumer now
exists (`difftest/src/census.rs`), and it folds the per-kind TALLIES onto the
ledger row — so the `outcome` split inside the largest kind is still unread.
**A count of a kind is not a worklist; the field that says which ones fired is.**

### A correction to the roadmap's description of the instrument

§2 says every row carries "`class`, `requester` (`file:line`),
`initiating_loader` and `reason`". That is true of
`compatibility-class-requested` rows and **not** of the other two kinds:

```text
compatibility-class-requested   class, requester, initiating_loader, reason, summary
native-shadows-bytecode         class, method, descriptor, native_kind, outcome, summary
synthetic-native-registered     class, method, descriptor, registered_by, summary
```

So a shadow row can be attributed to a TRIPLE but not to a call site, and the
retirement worklist above is a list of methods, not of source lines. Worth
knowing before planning work that assumes a `file:line` is available for the
226 the roadmap points at.

## Reproduce

```bash
cratonvm --java-home "$JDK" --jdk-only --explain-jdk-only --jdk-only-report r.json -cp probes/out Phase1Sweep
```

Three traps, all recorded in the roadmap's §5 and all still live: a dump flag
placed AFTER the main class is silently ignored (no file, no warning, exit 0);
the report path must be **Windows-shaped** on this host, or the VM prints
`os error 3`, continues, and the file never appears; and the report is **not
written when the program calls `System.exit`**.
