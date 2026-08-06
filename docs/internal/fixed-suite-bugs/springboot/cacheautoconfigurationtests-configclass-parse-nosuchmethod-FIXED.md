# `CacheAutoConfigurationTests`: config-class parse fails with a wrong-receiver `NoSuchMethodError`

**Status: FIXED 2026-08-05** — by `383e7f5cf` (`dev`), "a recycled
`JitInvokeInfo` address let one call site serve another's dispatch". Was
`docs/known-issues/springboot/cacheautoconfigurationtests-configclass-parse-nosuchmethod-gc-20260805.md`.

`module/spring-boot-cache` · `org.springframework.boot.cache.autoconfigure.CacheAutoConfigurationTests`

## Verdict

| arm | failures (of 59) |
|---|---:|
| HotSpot 25 control | 0 |
| CratonVM `--nojit` | 0 |
| CratonVM on `dev`, before `383e7f5cf` | 51–57 |
| **CratonVM on current `dev`** | **0** |

## CORRECTION (2026-08-05, later the same day)

**This page first named the wrong commit, and the way it got there is the
lesson.**

A 9-step `git bisect` named `836631dcc perf(jit): site-cache EVERY native from
compiled code, not just the leaves` — 53 failures at that commit, 0 at its
parent — and a single binary with `CRATONVM_JIT_SITE_CACHE` as the only
variable measured `all` at 53 failures and `leaf` at 0. Both measurements are
real and both reproduce. The conclusion drawn from them, that the widened site
cache was the defect, was **wrong**, and this page shipped a leaf-only default
on the strength of it.

`dev` landed `383e7f5cf` during the same investigation: every memo in
`jit/helpers.rs` is keyed on `(vm_identity, JitInvokeInfo pointer)`, those boxes
are freed with the `CompiledMethod` that owns them, and the allocator can hand
the same address to the next compile — so the key names a DIFFERENT call site
while the memo still holds the old site's answer. Widening the cache to hold a
resolved *native* made that latent defect reproducible on nearly every test,
which is why the bisect landed on the widening and why gating the widening
"fixed" it.

Re-measured on current `dev`, with `383e7f5cf` in, same class and fixture:

| mode | failures | when |
|---|---:|---|
| `all` | 53 | before `383e7f5cf` |
| `leaf` | 0 | before `383e7f5cf` |
| **`all`** | **0, twice** | **after `383e7f5cf`** |

The leaf-only default is therefore **reverted**; `all` is restored, so
836631dcc's win is not paid for a defect that no longer exists.

**A bisect names the commit that made a defect REPRODUCIBLE, which is not
always the commit that introduced it.** When the named commit is a same-day
perf change and the faces are wrong-object/wrong-dispatch, merge `dev` and
re-measure before gating it — the amplifier and the cause look identical from
one binary. See
[[reference_recycled_jitinvokeinfo_address_aliases_dispatch_memos]] and
[[reference_bisect_on_exposure_rate_defect_names_a_trigger]].

The rest of this page — the refutation of the GC hypothesis, and the census
technique that named the mechanism — stands unchanged and is what was actually
worth keeping.

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

Every per-call-site memo in `jit/helpers.rs` is keyed on `(vm_identity,
JitInvokeInfo pointer)`. Those boxes are freed with the `CompiledMethod` that
owns them, so the allocator can hand the same address to the next compile's
info — and the key then names a **different call site** while the memo still
holds the previous site's answer. `383e7f5cf` fixes that.

A memo holding a resolved *native* is the loudest form: the reusing site CALLs
the previous site's native and returns whatever that returns. So when
836631dcc widened the native site cache from leaf natives to every registered
native, it multiplied the number of memos carrying a callable target, and a
latent aliasing defect became a near-certainty on any class-loading-heavy
workload. That is why the bisect landed on the widening, and why gating the
widening made the symptom vanish.

The widening is not itself unsound, and the evidence that had suggested it was
— that `resolve_native_owner_for_receiver` walks the receiver's superclass
chain while `invoke_or_native` resolves on `effective_class` alone — was never
tied to an observed wrong dispatch. It is worth keeping in view as a question
about the two resolvers, but it is not what broke this class.

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

## What the mode lever measured, and what it did NOT prove

`CRATONVM_JIT_SITE_CACHE` was added to take the difference apart, and is kept.
On the pre-`383e7f5cf` binary, one fixture, the mode as the only variable:

| mode | failures (pre-`383e7f5cf`) |
|---|---:|
| `all` — 836631dcc | 53 |
| `no-super-walk` | SIGSEGV mid-run |
| `leaf` | **0** |
| `off` | **0** |

Read at the time as "the admission rule is the defect". Read correctly, it is a
**dose-response curve on how many memos hold a callable target**: `all` fills
the most, `leaf` far fewer, `off` none — and `no-super-walk`, which narrows the
*resolution* but not the number of memos, stays broken. That last row was
treated as evidence the walk was innocent; it is better evidence that
resolution was never the axis at all.

On current `dev` the whole curve collapses: `all` measures **0 failures,
twice**. Nothing about the admission rule changed in between — only
`383e7f5cf`.

**A monotone response to a knob does not tell you the knob is the defect.**
It can equally mean the knob controls exposure to something else.

Post-fix census, against both prior arms:

| native | current `dev` | pre-`383e7f5cf` (JIT) | `--nojit` |
|---|---:|---:|---:|
| `Function$Identity.apply` | **0** | 5 870 | **0** |

The compiled arm agrees with the interpreter exactly, which is the property
that was violated — and it does so with the site cache at its full `all`
default, which is the point.

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

On current `dev` this class is green in every mode; the split below only
reproduces on a binary built **before** `383e7f5cf`:

```bash
CRATONVM_JIT_SITE_CACHE=all   # 53 failures pre-383e7f5cf, 0 after
CRATONVM_JIT_SITE_CACHE=leaf  # 0 failures either way
```

against `module/spring-boot-cache`'s
`org.springframework.boot.cache.autoconfigure.CacheAutoConfigurationTests`.
The classpath is ~35 KB and must go through a `@argfile`; a bare `-cp` exceeds
the Windows command-line limit and `Start-Process` fails with "The filename or
extension is too long".
