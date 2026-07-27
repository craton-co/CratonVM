# `is_known_miscompile` — RETIRED: block deleted, bans lifted, entries verified

**Status: ✅ CLOSED 2026-07-27.** The ~950-line `is_known_miscompile`
`matches!` block in `vm/src/jit/skip_list.rs` (189 `(class, method)` entries,
~25 named historical bans) and this file's own private
`callee_saved_gpr_local_homes_enabled()` gate are **deleted**. Supersedes
`docs/known-issues/jit-bans/is-known-miscompile-block-inert-20260726.md`, whose
conclusion ("consolidated finding, no code change") is now carried out, plus
the caveat that doc left open ("this does NOT mean the underlying miscompiles
are fixed") is answered below with direct evidence rather than a reachability
argument alone.

Landed together with two bug fixes found while verifying it — see
[Bugs found and fixed](#bugs-found-and-fixed).

## 1. The gate had drifted from the feature it was named after

`a4913d8b` ("disable callee-saved GPR local homes") put the *entire* targeted
list behind a switch, on the premise that every entry was one
register-allocator family that the switch turned off.

There are **two different functions with that name**:

| | default | what it controls |
|---|---|---|
| `jit::x64::callee_saved_gpr_local_homes_enabled()` | **true** (`unwrap_or(true) && (precise_jit_maps_enabled() \|\| moving_young_enabled())`) | the actual register allocator strategy |
| `vm::jit::skip_list::callee_saved_gpr_local_homes_enabled()` (private, now deleted) | **false** on x86_64 | whether the ban list fires |

`precise_jit_maps_enabled()` has been default-on since 2026-07-07, so the real
allocator switch has been **ON** for weeks — while the skip-list's private copy
of "the same" switch kept **every entry inert** the whole time. Every suite this
repo runs (Spring, Spring Boot, Tomcat, H2, WildFly, Elasticsearch, Keycloak)
has therefore been executing with the allocation strategy these bans were meant
to contain *active* and the bans themselves *off*, at their current documented
pass rates.

Note the direction: the skip-list gate defaulting `false` means the bans were
*never applied*. Deleting them changes no default behaviour on x86_64. On
non-x86_64 that gate returned `true`, but the JIT emits x86-64 machine code
(`jit/src/lib.rs` guards its emitters with `#[cfg(target_arch = "x86_64")]` and
its own tests note "on non-x86_64 targets the emitted bytes are not valid"), so
there is no working JIT there for the list to protect either.

## 2. Direct verification, not just a reachability argument

A purpose-built probe set (12 programs, all validated against real HotSpot
first) exercised the named families with oracle-checked results, run three ways
against one frozen binary: `--nojit`, default JIT, and
`CRATONVM_JIT_THRESHOLD=1`. `CRATONVM_DBG_JITC=1` recorded which methods were
*actually* compiled, so a clean result can be attributed to "compiled and
correct" rather than "never compiled, so the probe proved nothing" — the
mistake recorded in `jit-ban-shadowed-differential-false-positive`.

**19 of the 189 entries were confirmed genuinely JIT-compiled today and
correct in all three configurations:**

| Named ban | Entries verified compiled + correct | Probe |
|---|---|---|
| **KC26.LR** | `io/smallrye/config/…$ConfigValueProperties$LineReader.readLine`, `…$ConfigValueProperties.load0` (both) | `SmallryeLineReaderProbe` — real smallrye-config 3.16.0, 150 rounds over a 67-key properties file with continuations/escapes/unicode/CRLF, oracle = `java.util.Properties` |
| **KC-CRED.LAZY** | `PasswordCredentialData.getAdditionalParameters`, `PasswordSecretData.getAdditionalParameters` (both) | `KeycloakCredProbe` — real keycloak-server-spi 26.6.1, 2000 rounds incl. `PasswordCredentialModel.createFromValues` |
| **ecj `HashtableOf*.rehash`** | `HashtableOf{Int,Long,Object,ObjectToInt,Type,Module}.rehash` (6 of 7) | `EcjHashtableProbe` — real ecj 3.32.0, tables grown 4 → 600 entries so `rehash` runs repeatedly, every entry read back |
| **BC-ASN1.1** | `java/util/Calendar.isFieldSet` | `BoxStringProbe` |
| **ES-HANG-01** | `WeakHashMap$ValueSpliterator.tryAdvance`, `WeakHashMap$HashIterator.hasNext`, `WeakHashMap${Key,Value,Entry}Iterator.next`, `WeakHashMap.getTable` | `WeakRefProbe` |
| **EXEC.1** | `LinkedBlockingQueue.{offer,take}` | `ExecutorProbe` — real `ThreadPoolExecutor`, 4 threads × 1500 tasks, blocking producer/consumer |

The 7th ecj entry, `HashtableOfPackage.rehash`, has no `rehash` in ecj 3.32.0
(the class extends `CharDelegateMap` there); it belongs to an older ecj.

**The remaining 170 entries cannot be JIT-compiled in this VM at all**, for
reasons independent of the deleted gate:

- **Rust natives win over bytecode.** Every `java/util/HashMap` /
  `LinkedHashMap` / `HashSet` / `ImmutableCollections` entry, `String.hashCode`
  / `toLowerCase` / `toUpperCase`, `Integer.parseInt`, `Long.parseLong`,
  `Class.getDeclaredFields` / `getGenericInterfaces`, `Reflection.filter*`,
  `ByteBuffer.allocate`, `AtomicInteger.*`, `Reference.clear` and friends are
  implemented natively (`native-collections`, `native-builtins`), so their
  bytecode never executes. A `CRATONVM_DBG_JITC` census over
  `MapFamilyProbe`/`BoxStringProbe`/`ReflectProbe`/`MiscJdkProbe` shows **zero**
  compile events for any of them, while the same runs happily compile the
  *user* and *library* code around them. (This also means the historical
  evidence behind e.g. SPB.1's `HashMap.putVal` entry can no longer be
  reproduced through that method at all.)
- **`<init>` entries** (`Integer`, `Long`, `HashSet`, `StringJoiner`,
  `HashMap$HashIterator`, `WeakHashMap$Entry`,
  `SpringIterableConfigurationPropertySource$CacheKey`) are already caught by
  the generic non-trivial-constructor gate in `should_skip_jit_with_init`.
- **Shadowed by separate, still-active blanket bans**: all 16
  `org/springframework/boot/context/properties/source/…` entries (SPB.2/SPB.3,
  covered by the live `org/springframework/boot/context/` and
  `org/springframework/boot/` bans), `ByteBuddyState.make` (HIB-PROXY, covered
  by the live `org/hibernate/` ban — HIB-TEMPORAL.1, re-confirmed needed
  2026-07-26), `junit/textui/TestRunner.main` (live `junit/` ban).
- **No fixture on this host**: `org/apache/felix/framework/util/SecureAction.
  lambda$getAccessor$0` (FELIX.1) — no Felix jar anywhere on the build host.
  Its sibling entry `AccessibleObject.setAccessible` was exercised
  (`ReflectProbe`, incl. the bulk `setAccessible(AccessibleObject[], boolean)`
  overload); the underlying `AccessibleObject.setAccessible0` compiles and is
  correct.

The remaining `org/springframework/util/`, `org/springframework/core/env/` and
`org/springframework/beans/` entries were exercised directly by
`SpringUtilProbe` (real spring-core/spring-beans 7.0.8): `StringUtils.
toStringArray`, `ObjectUtils.nullSafe{Equals,HashCode}`, `MapPropertySource.
{getPropertyNames,getProperty,containsProperty}`, `ConcurrentReferenceHashMap`
+ `$Segment`, and `ExtendedBeanInfo$PropertyDescriptorComparator.compare` — see
the caveat in §4, that probe hits an unrelated pre-existing VM gap partway
through and its coverage is partial.

`org/apache/tomcat/util/buf/MessageBytes.newInstance` (+ its factory) was
verified separately: 20 000 rounds of allocate / set bytes / set chars / set
string / `toString` / `equalsIgnoreCase` / `recycle` against real
`tomcat-util.jar`, clean in all three configurations.

## 3. Bugs found and fixed

### (a) JIT: a code-buffer overflow aborted the whole VM instead of falling back

`groovy/lang/GroovyClassLoader.doParseClass` is one of the deleted entries, so
`TomcatGroovyProbe` parses 25 Groovy classes through the real
`groovy-3.0.21.jar`. Under `--nojit`: clean. Under default JIT **and**
`CRATONVM_JIT_THRESHOLD=1`: the process **aborted**.

```
thread 'main-vm' panicked at jit/src/ir_lower.rs:2740:18:
call-exc JE patch in-bounds: PatchFailed { kind: "i32", offset: 4125 }
fatal runtime error: failed to initiate panic, error 5, aborting
```

`ExecutableBuffer::try_patch_i32` documents that it marks the buffer
`overflowed` and returns `Err` *instead of panicking*, and that "the caller may
ignore the `Err` and rely on that bail" — `lower_inner` discards the whole
`CompiledMethod` when `buf.overflowed()` and the caller falls back to the
single-pass backend. `emit_deopt_stub` and `emit_call_exc_stub` violated that
contract with `.expect(...)`. They run *before* the bail-out, on a VM thread
with no unwinding catch, so a method body that outgrew its estimated buffer
took the whole process down. (`offset == len` in the panic: the branch's own
`rel32` placeholder had already been dropped by the sticky-overflow `emit`.)

Fixed in `jit/src/ir_lower.rs` by routing both stub patch loops through a new
`patch_or_bail` helper that honours the documented contract — the same
convention `patch_rel32_to_here` already followed. A `debug_assert!` keeps the
invariant loud in debug builds: a patch failure must have marked the buffer
overflowed, which is exactly what makes the compile discardable.

This defect is **not** specific to Groovy — any method whose lowered body
exceeds `nodes*32 + calls*448 + 1024` bytes reaches it. Groovy's AST/parser
code is simply the shape that got there first in a probe.

### (b) `dev` did not compile

Unrelated to this task but blocking it: dev merge `bf2579d96` dropped both
halves of `462c51d00`'s `NativeContext::invoke_special_by_class_id` (the trait
declaration and the `NativeContextImpl` impl) while keeping its caller in
`native-builtins/src/lang_class.rs`, so `cargo build` failed with `E0599` for
every session. Restored verbatim in commit `fd29d3cd2` and pushed to dev
immediately.

## 4. Pre-existing VM gaps surfaced by the probes (NOT JIT, NOT fixed here)

Each of these behaves **identically** with the JIT on, with
`CRATONVM_JIT_THRESHOLD=1`, and under `--nojit`, so none is JIT-attributable
and none affects the conclusion above. They are recorded here because two of
them cut a probe short, and because they look like genuine real-JDK-mode gaps
worth their own investigation:

1. **`ReferenceQueue.enqueue` NPEs**: `java/lang/NullPointerException: Cannot
   enter synchronized block because "this.lock" is null`, thrown from
   `ReferenceQueue.enqueue` (real JDK 25 bytecode, `--java-home`). Hit directly
   by `WeakRefProbe` (`Reference.enqueue`) and indirectly by `SpringUtilProbe`
   via `ConcurrentReferenceHashMap`'s soft-reference cleanup
   (`SoftEntryReference.release` → `Segment.doTask` → `remove`). The latter
   truncates `SpringUtilProbe` at its `ConcurrentReferenceHashMap.remove` loop,
   so that probe's coverage of the Spring entries is partial — the sections
   before it (`StringUtils`, `MapPropertySource`, CRHM `put`/`get`/`containsKey`)
   did run clean in all three configurations.
2. **`java.security.Provider` alias/lookup gaps**: `Alg.Alias.<type>.<alg>`
   registration does not resolve to the aliased service, case-insensitive
   algorithm lookup misses, `putService` aliases miss, a `remove`d legacy
   service still resolves, and `Security.getProvider("SUN")` has no
   `MessageDigest.SHA-256` service (`MessageDigest.getInstance("SHA-256")`
   itself works). 3600 gap-hits + 300 hard failures per `ProviderProbe` run,
   bit-identical across all three JIT configurations.
3. **Custom `java.util.logging.Level` registration**: `Level.parse("PROBE0")`
   throws `IllegalArgumentException: Bad level` for a `Level` subclass created
   in-process, i.e. `Level$KnownLevel`'s registration path (whose
   `lambda$add$0/1` are two of the deleted entries) does not take effect.

## 5. What was deliberately NOT removed

- `is_known_miscompile_aqs_family` (AQS / `AbstractQueuedLongSynchronizer` /
  `ReentrantLock` / `ReentrantReadWriteLock`) — unconditional, re-confirmed
  reproducing 2026-07-23 (H2 `TestFileSystem.testConcurrent`).
- `is_known_miscompile_clq_family` (`ConcurrentLinkedQueue`) — unconditional.
- `is_unconditional_hash_miscompile_cluster` — still carries the
  `Arrays.hashCode` / `Objects.hash` / `Objects.hashCode` /
  `ArraysSupport.hashCode` entries that the deleted list also listed, so those
  four are *not* lifted by this change.

Those three are precisely the entries that turned out **not** to belong to the
callee-saved-GPR-local-home family, which is why the 2026-07-26 finding
correctly stopped short of deleting the block wholesale.

## 6. Reproducing

Probe sources, driver and per-run logs on the Linux build host:
`/data/jbi-probes/` (`run-probes.sh <binary> <tag> [probe…]`,
`census.sh <binary>` for the `CRATONVM_DBG_JITC` compile census,
`ban-pairs.txt` for the extracted 189-entry inventory). The frozen baseline
binary used for the pre-change runs is `/data/cratonvm-jbi-base-20260727`.
