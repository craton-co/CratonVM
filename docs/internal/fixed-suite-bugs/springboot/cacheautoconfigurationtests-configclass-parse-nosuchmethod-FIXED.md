# `CacheAutoConfigurationTests`: config-class parse fails with a wrong-receiver `NoSuchMethodError`

**Status: FIXED 2026-08-05** — `fix(jit): the native site cache may serve LEAF
natives only`. Was
`docs/known-issues/springboot/cacheautoconfigurationtests-configclass-parse-nosuchmethod-gc-20260805.md`.

`module/spring-boot-cache` · `org.springframework.boot.cache.autoconfigure.CacheAutoConfigurationTests`

## Verdict

| arm | failures (of 59) |
|---|---:|
| HotSpot 25 control | 0 |
| CratonVM `--nojit` | 0 |
| CratonVM on `dev`, before the fix | 51–57 |
| **CratonVM on `dev`, after the fix** | **0** |

One defect, in the JIT: `836631dcc perf(jit): site-cache EVERY native from
compiled code, not just the leaves`, named by a 9-step `git bisect` (53
failures at that commit, 0 at its parent).

## What the original page got wrong, and why it is worth saying

The page led with a GC hypothesis — "a live lambda/proxy object backing this
`BiConsumer` call was collected while still reachable" — on the strength of two
`cratonvm::gc::guard` errors that named the same call site as the
`NoSuchMethodError`. It was wrong, and three measurements say so:

* a crashing arm's fatal-error report read **`gc young-gen actual: 0 moving
  cycle(s), 0 cycle(s) diverted to the NON-MOVING sweep`** — the corruption
  happens with no young collection at all;
* `CRATONVM_GC=-moving-young` still failed 55 of 59;
* `--Xmx 12g`, which collects far less, still failed 51 of 59.

The guard was not lying, it was answering a narrower question than it appeared
to. `young_freed_lookup` reports that an address lies inside a young span the
non-moving sweep once freed and coalesced — and the allocator **re-serves**
those spans, so a freshly-allocated object sitting in one reads identically to a
prematurely-reclaimed one. The receiver really did have an all-zero header and
really was inside a formerly-freed span; neither fact implied a collector bug.
**A reclamation-ring hit is evidence about an ADDRESS's history, not about the
OBJECT currently at it.**

## The defect

The JIT keeps a per-call-site native resolution cache: resolve the target once,
then dispatch it directly and skip `invoke_or_native`. 836631dcc widened
admission from leaf natives to *every* registered native, arguing that skipping
`invoke_or_native`'s ~27-gate cascade is worth more than skipping the funnel.
The arithmetic was right — the measured rungs are real — but the premise was
not.

`invoke_or_native` resolves the native on `effective_class` and **nothing
else**, behind gates keyed on class and receiver shape: annotation proxies, the
`ClassLoader` built-in override, the spring-boot loader helpers, the
`MethodHandle` downcall forms, and more. The cache's own
`resolve_native_owner_for_receiver` instead starts at the **receiver's runtime
class** and walks its superclass chain. Those are different questions, so a
compiled call site could dispatch a native the interpreter would never reach.

The commit's own text claims the walk "reproduces `invoke_or_native`'s rule
exactly, including its exception". It reproduces the rule that the rest of the
dispatch pipeline implements across several steps — not the one function whose
work it is skipping.

## How it was caught: `--dump-native-registry`, both arms, diffed

Reasoning about which of ~12 000 registrations could misfire was going nowhere.
The census settles it in one comparison — the same class, the same fixture, JIT
on versus `--nojit`, per-native invocation counts:

| native | JIT | `--nojit` |
|---|---:|---:|
| `java/util/function/Function$Identity.apply(Object)Object` | **5 870** | **0** |

`native_function_identity_apply` returns `args[1]` — its first argument. The
JIT arm dies at 57/59 and therefore does strictly *less* total work, and it
still dispatched 5 870 of these while the interpreter dispatched none.

That is the whole "impossible" symptom set, in one line. Downstream it reads as:

* `StreamSupport.stream(spliterator, false)` handing back the **spliterator**,
  so `elementStream()` returns a `Spliterators$IteratorSpliterator` and
  `ClassFileAnnotationMetadata.of`'s `Stream.forEach` raises
  `NoSuchMethodError: java.util.Spliterators$IteratorSpliterator.forEach` —
  the page's item 1, in its local dress;
* `ResolvableType.forMethodParameter(mp)` handing back the `MethodParameter`,
  so `getTypeForFactoryMethod` stores one in a `ResolvableType` field;
* `Proxy$Dispatch.invokeProxy` reached with a null `Method`;
* a Hazelcast XSD parsed as `SAXParseException` at line 353, and
  `Class.getMethod: name is null` — the same wrong-object family wearing
  whatever face the next dereference gives it.

**A call that returns its own first argument is a dispatch defect, not a memory
defect.** Every face above is one object standing where another belongs, which
is what a wrong *target* looks like — and nothing about it requires a collector.

## The fix, and why it is the admission rule rather than the walk

Leaf-only admission, as before 836631dcc. The leaf claim is made per
registration by someone who checked that the native may skip the funnel; that
audit is exactly what the non-leaf registrations have not had.

`CRATONVM_JIT_SITE_CACHE` was added to take the difference apart, and kept.
One binary, one fixture, the mode as the only variable:

| mode | failures |
|---|---:|
| `all` — 836631dcc | 53 |
| `no-super-walk` | SIGSEGV mid-run |
| `leaf` — the new default | **0** |
| `off` | **0** |

`no-super-walk` is the load-bearing row: restricting the resolver to rule 1 —
a native registered on the dispatch class itself, which *is* what
`invoke_or_native` does — still crashes. Narrowing the resolver does not
restore the gate cascade, so the walk was never the fix.

Everything else 836631dcc landed stays: the superclass walk (sound for leaves),
the counters, the rename. The leaf fast path is unharmed —
`CRATONVM_DBG=intrinsic-stats` on an `AtomicInteger.get` loop reports
**1 995 000** compiled leaf dispatches after the fix, against the 1 995 028 the
commit itself recorded.

Post-fix census, against both prior arms:

| native | fixed | pre-fix (JIT) | `--nojit` |
|---|---:|---:|---:|
| `Function$Identity.apply` | **0** | 5 870 | **0** |

The compiled arm now agrees with the interpreter exactly, which is the property
that was violated.

## Residuals — NOT closed by this fix

1. **The page's item 3, the Infinispan verifier rejection.** `retransformClasses0`
   rejecting ByteBuddy's `ConfigurationBuilder.simpleCache` for "stack overflow
   during verification" at offset 27 **never reproduced on Windows**, at any
   commit tested, including the pre-836631dcc anchors that are contemporary with
   the Azure run the page was written from. All 59 tests pass here, so the
   `mock(ConfigurationBuilder.class)` retransform succeeds. Filed separately as
   `docs/known-issues/springboot/infinispan-configurationbuilder-retransform-verify-20260805.md`
   with the diagnostics needed to settle it the next time it appears.

2. **29 `SyntheticStub` natives dispatched while the real JDK bytecode for the
   same method is loaded** — `StreamSupport.stream` (1 922 calls, a registration
   that `overwrote` a bridge), `AtomicBoolean.compareAndSet` (239) and 27 more.
   `AtomicBoolean` is explicitly on `real_protected_stub_class_common`'s
   yield-to-real-bytecode list, so something is bypassing that arbitration.
   Present under `--nojit` too, which passes, so it is not this defect — but it
   is a real one. Found by the same census; the list is reproducible with
   `--dump-native-registry`.

3. **The page's Azure-only faces were never reproduced on Windows.** The
   `java.lang.Object.accept(Object,Object)V` spelling and the two
   `gc::guard` lines come from a run on `dev @1078f6f05c` (11:31 UTC), which
   *predates* 836631dcc (12:10 UTC). At the local commits contemporary with it
   this class was green (0–1 failures across five bisect steps). The Azure
   checkout also carries ~12 788 CRLF-corrupted fixture files (see
   `RESULTS-20260805-azure-fullsuite.md`), which is its own confound. The next
   full-suite run is the check that matters; if the class is green there, this
   is closed, and if it is not, the failure is a different one and deserves its
   own page rather than this one reopened.

## Reproducing

```bash
CRATONVM_JIT_SITE_CACHE=all   # 53 failures
CRATONVM_JIT_SITE_CACHE=leaf  # 0 failures — the default
```

against `module/spring-boot-cache`'s
`org.springframework.boot.cache.autoconfigure.CacheAutoConfigurationTests`.
The classpath is ~35 KB and must go through a `@argfile`; a bare `-cp` exceeds
the Windows command-line limit and `Start-Process` fails with "The filename or
extension is too long".
