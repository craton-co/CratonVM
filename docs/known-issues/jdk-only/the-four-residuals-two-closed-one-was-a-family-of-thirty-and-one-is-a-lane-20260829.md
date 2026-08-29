# The four L7 residuals: two closed, one was a family of thirty, one is a lane

**Status: MEASURED 2026-08-29** on `azure-host-2` (`azureuser@20.80.105.49`),
worktree `/data/cvm-l7dod-20260828`, oracle HotSpot
`/data/jdkimages/jdk25-linux/jdk-25.0.4+7`.

Follow-up to
`the-definition-of-done-run-on-the-three-real-workloads-20260828.md` §8, which
recorded four residuals with the reason each was not fixed. Three of the four
reasons did not survive contact.

| | residual | outcome |
| --- | --- | --- |
| R1 | `SSLSocket.startHandshake()` renegotiation | **WILL NOT FIX, now with the reason verified** — plus the silent no-op made audible |
| R2 | compat's `ArrayList.toArray(T[])` had no store check | **CLOSED — and it was 30 classes, not one, plus a second defect underneath** |
| R3 | compat's `ServiceLoader.iterator()` wrong type | **CLOSED by retiring the shadow** |
| R4 | FFM carriers are instances of an INTERFACE | **NOT FIXED — it is a lane. Measured properly and given a standing assertion** |

---

## R1 — verified, not repeated

The earlier record said the vector cannot pass because rustls implements no TLS
1.2 renegotiation. True, and incomplete: it never established that *nothing the
VM could truthfully report* would change the verdict. It does not.

```java
// TesterSupport.isClientRenegotiationSupported
String sslImplementation = (String) tomcat.getConnector().getProperty("sslImplementationName");
if (!JSSEImplementation.class.getName().equals(sslImplementation)) return false;
return true;
```

The expectation keys on a **Tomcat configuration property** and asks the running
platform nothing. So for the `[JSSE]` arm it is `true` unconditionally, and the
only ways to satisfy the assertion are to perform a real renegotiation, or to
fire `HandshakeCompletedEvent` without one. rustls declines the first on
security grounds — its own manual lists the omission as its mitigation for
CVE-2009-3555 and 3SHAKE — and the second is a lie about key material.

**What was improvable is the silence.** `ensure_layered_handshake_started`
returns immediately for a socket whose `tls_id` is already a live rustls id, so
`startHandshake()` on an established connection succeeded, changed nothing, and
said nothing — and forcing fresh key material is the only reason to call it
there. It now emits one `warn!` per process naming what was not done and why.
Behaviour is unchanged; the vector stays red; a debugging session is saved.

`warn!`, not `debug!`, because `debug!` is compiled out of shipping builds and
would not be a signal at all.

## R2 — the reason for deferring was real, and removing it exposed a family

The recorded reason was that the message renderer lived in `native-builtins`,
which depends on `native-collections` and not the reverse. That was true. The
fix is to move it **down**, not to copy it: `element_type_mismatch` and
`external_class_name` now live in `cratonvm_types::error::arraycopy_message`,
beside the eight sibling messages (`source_index`, `type_mismatch`,
`destination_not_an_array`, …) that were already there, with the measured
HotSpot wording pinned by two new unit tests.

Then three things went wrong in a row, and each is the useful part.

### 1. The first fix was inert — `owns_slot` checked one step too late

```text
java/util/ArrayList.toArray([Ljava/lang/Object;)   synthetic-stub
  native-collections/src/lib.rs:5110   invocations 0  owns_slot FALSE   <- what I edited
  vm/src/vm/vm_init.rs:3259            invocations 5  owns_slot TRUE    <- what runs
```

The scope brief names this trap and I walked into it anyway, by editing the
obvious file before dumping the registry. The losing twin keeps the fix — a
duplicate pair sitting half-fixed is how one of the two gets repaired alone and
the other survives until registration order changes.

### 2. It is a FAMILY: 32 registrations over 30 classes from nine sites

```text
total registrations of toArray([Ljava/lang/Object;)  32
  java/util/ArrayList, ArrayList$SubList, AbstractCollection, HashSet,
  LinkedHashSet, LinkedList, TreeMap$Values, TreeMap$EntrySet,
  HashMap${Key,Entry}Set, HashMap$Values, Hashtable${Key,Entry}Set,
  Hashtable$ValueCollection, LinkedHashMap$Linked{Key,Entry}Set,
  LinkedHashMap$LinkedValues, ConcurrentHashMap${Key,Entry,Values}*,
  CopyOnWriteArraySet, Collections$Synchronized{Collection,Set},
  cratonvm/internal/Unmodifiable* ...
```

Patching them one at a time would have been seven copies of one twelve-line
check — the shape `appended_slots` was written to collapse fifteen copies of,
and `instantiable` another. So the check is now one function,
`native-api/src/array_store.rs`, called from all five implementations that
actually copy (the SubList, Unmodifiable and Synchronized registrations delegate
to a backing collection and inherit it).

**The route is an argument, because the JDK throws from two places and the text
differs.** MEASURED:

```text
ArrayList.toArray(new String[0])    arraycopy: element type mismatch: can not cast one of
                                    the elements of java.lang.Object[] to the type of the
                                    destination array, java.lang.String
ArrayList.toArray(new String[2])    the same sentence
HashSet.toArray(new String[0])      java.lang.Integer
LinkedList.toArray(new String[0])   java.lang.Integer
```

`ArrayList.toArray(T[])` copies through `Arrays.copyOf` / `System.arraycopy`;
`AbstractCollection.toArray(T[])` stores through `aastore` in its own loop. One
probe case could not have shown that, and a single message would have been
wrong for half the family.

### 3. `LinkedList` still did not throw — and the reason was a worse defect

With the check in place `LinkedList` stayed silent. Not a bug in the check: when
the caller's template was too small, `native_ll_to_array_typed` allocated
`alloc_ref_array(ctx, size)` — **a bare `Object[]`** — instead of an array of
the template's runtime component type. Every reference is storable in an
`Object[]`, so the check was correct to stay silent; the wrong answer was one
step earlier, and it is the more serious of the two: `Collection.toArray(T[])`
is specified to return the template's type, and a caller assigning to `String[]`
gets a `ClassCastException` at the `checkcast`. Both siblings already did it
right.

**The probe could not see it either, and that is worth more than the fix.**
`List<Object> ll` makes `ll.toArray(new String[0])` statically `Object[]`, so
javac emits no `checkcast` and the wrong runtime type never surfaces. The sweep
now prints the RETURNED ARRAY'S CLASS for eight `toArray` receivers — a claim
that is independent of the store check and would have found this alone.

**Result: `probes/DodArrayStoreSweep` is 0-diff against HotSpot in BOTH modes**,
across 60+ cases covering both polarities, both routes, both message shapes, the
result type, and array-of-array components.

## R3 — closed by retiring the shadow, not by improving it

```text
ServiceLoader.load(SLF4JServiceProvider.class, cl).iterator().getClass()
  HotSpot   java.util.ServiceLoader$2
  compat    java.util.ArrayList$Itr        <- before
  compat    java.util.ServiceLoader$2      <- after
```

The nine `java/util/ServiceLoader` natives are gone from real-JDK builds and
remain under `--features synthetic-jdk`, where there is no bytecode to run.

Writing a lazier stub would have been re-implementing a JDK class to match a JDK
class. The registrar says so itself — *"`java.util.ServiceLoader` is pure
Java"* — and the pure-Java path is not a hope: `--jdk-only` refuses every
SyntheticStub, so it has been running the real `ServiceLoader` all along, and
after the class-path-module fix it is HotSpot-identical on both SPIs and
completes all five definition-of-done workloads. `jdbc` (92/92) and `h2jdbc`
(12/12) are `DriverManager` discovery — `ServiceLoader.load(java.sql.Driver.class)`,
**the exact case `jdbc.rs` says these natives exist for.** The shadow's own
justification is discharged by the mode that refuses it.

The gate goes inside the registrar because it has two callers
(`vm_init::init_service_loader_bootstrap` and `jdbc::register_jdbc_service_loader`)
and the dump shows every row twice, once from each.

`jdbc::tests::driver_natives_registered` went red, correctly. It is split rather
than deleted: the synthetic-jdk half asserts the nine are present as before, and
a new real-JDK half asserts they are **absent** — the retirement itself, which
would otherwise be pinned by nothing. A retirement with no test is a change that
comes back.

## R4 — not fixed. It is a lane, and it is seven sites, not one

The earlier record described one fabrication request whose fallback allocates
against `java.lang.foreign.MemorySegment`, the interface. A probe written for
the general species found **seven**, and one of them is wrong in **compatible
mode too**:

```text
                                    HotSpot                              CratonVM --jdk-only
Arena.ofConfined().allocate(16)     jdk.internal.foreign.NativeMemorySegmentImpl   MemorySegment  INTERFACE
Arena.global().allocate(8)          NativeMemorySegmentImpl                        MemorySegment  INTERFACE
Arena.ofConfined()                  jdk.internal.foreign.ArenaImpl                 Arena          INTERFACE  <- both modes
MemorySegment.ofArray(byte[16])     HeapMemorySegmentImpl$OfByte                   MemorySegment  INTERFACE
MemorySegment.ofArray(int[4])       HeapMemorySegmentImpl$OfInt                    MemorySegment  INTERFACE
MemorySegment.NULL                  NativeMemorySegmentImpl                        MemorySegment  INTERFACE
...allocate(16).asSlice(4)          NativeMemorySegmentImpl                        MemorySegment  INTERFACE
```

JVMS §6.5 makes `new` on an interface or abstract class an
`InstantiationError`, so each of these is a receiver no bytecode in any image
could have produced — a defect on its own terms, needing no oracle.

### The instrument gap this exposes

`--jdk-only`'s predicate is `compatibility_classes: 0`, which counts classes
**minted**. An instance allocated against a REAL class that happens to be an
interface is invisible to it. Both statements are true of the `tcnetssl` arm at
once: the DoD predicate holds and the object model is violated.

### The gap is now closed — and the population is 31, not 7

`try_alloc_concurrent_synthetic`, the funnel natives use to mint stand-in
receivers, now asks `instantiable`'s predicate and warns once per class when the
answer is no. It is the funnel both the segment fallback and the `Arena`
carriers go through, it already reads `class_num_total_fields` unconditionally,
and it is already `#[track_caller]` — so the requester `file:line` costs
nothing, and this is not on the per-object hot path that `layout_alias` has to
buy a flag to observe.

Run over the five definition-of-done arms under `--jdk-only`, it names **31
distinct classes**, on every one of which the same report says
`compatibility_classes: 0`:

```text
java/nio/file/Path                     interface  phases_late/nio_file.rs:9429
java/nio/file/FileSystem               abstract   phases_late/nio_file.rs:12229
java/nio/file/spi/FileSystemProvider   abstract   phases_late/nio_file.rs:13334
java/nio/file/attribute/FileAttribute  interface  phases_late/nio_file.rs:22845
java/lang/invoke/MethodHandle          abstract   lang_invoke.rs:9894
java/lang/invoke/VarHandle             abstract   lang_invoke.rs:2821
java/lang/foreign/Arena                interface  phases_late/foreign_ffm.rs:864
java/lang/foreign/MemorySegment        interface  panama.rs:124
jdk/internal/foreign/MemorySessionImpl abstract   phases_late/foreign_ffm.rs:759
java/lang/reflect/ParameterizedType    interface  generics.rs:519
java/lang/reflect/GenericArrayType     interface  generics.rs:730
java/lang/reflect/TypeVariable         interface  generics.rs:896
java/lang/StackWalker$StackFrame       interface  phases_late/reflect_invoke.rs:2641
java/security/MessageDigest            abstract   jca/message_digest.rs:235
java/security/Provider                 abstract   jca/provider_chain.rs:338
java/security/PrivateKey               interface  keystore.rs:2883
javax/net/ssl/SSLSocket                abstract   net_phase_e.rs:15683
javax/net/ssl/SSLSocketFactory         abstract   net_phase_e.rs:15264
javax/net/ssl/SSLSession               interface  t27_tls.rs:15857
javax/net/ssl/SSLSessionContext        interface  net_phase_e.rs:1035
java/util/stream/Stream                interface  phases_late/reflect_invoke.rs:2731
java/util/stream/IntStream             interface  lang_string.rs:13095
java/util/ResourceBundle               abstract   locale_resources.rs:1780
java/net/JarURLConnection              abstract   net_phase_e.rs:10631
java/lang/Runnable                     interface  t27_tls.rs:16429
… and the six java.lang.management / com.sun.management MXBean interfaces
```

**Two numbers, two questions, and they must not be conflated.** The census
counts ALLOCATIONS: some of these objects may never escape the native that made
them, and an interface-classed object nobody outside sees is a latent defect
rather than a live one. `probes/AbstractReceiverSweep`'s **7** are the ones a
FACTORY hands back — confirmed reachable from application code. The census says
where to look; the probe says which ones a program can already trip over.

What this does not do is put the species in the report. A new `JdkOnlyViolation`
variant is a wire-format change — `kind()` is the documented `"kind"` field of
every row, `difftest/src/census.rs` tallies by exactly that string, and
`jfr/src/jdk_only.rs` holds a closed label vocabulary with its own schema
version. Four crates and a schema bump, and a half-added kind (recorded but not
tallied, or tallied but not labelled) is worse than none. `warn!` reaches
stderr, which `probes/dodscreen-linux.sh` already keeps per arm, so the screen
can see it today; promoting it to a row is the next step and now has a
population to justify it.

### Why the FFM carriers are still not fixed here

The tree already has both primitives — `instantiable::first_instantiable` to
pick a concrete class, and `appended_slots::base_for_class` to keep private
state above a real class's declared fields. What blocks the change is the slot
maps:

```text
native-builtins/src/panama.rs          321 get_field/set_field with absolute indices
native-builtins/src/phases_late/foreign_ffm.rs, panama_libffi.rs, phases_early.rs,
  lang_invoke.rs                       arena and segment carriers
vm/src/jit/helpers.rs                  a JIT intrinsic that reads segment slots
```

Adopting `NativeMemorySegmentImpl` (4 declared fields: `length`, `readOnly`,
`scope`, `min`) or `ArenaImpl` (2: `session`, `shouldReserveMemory`) means
rebasing every one of those onto an appended-slot base, including offsets baked
into compiled code. That is a lane, and doing half of it is worse than either
endpoint.

### What is delivered instead

`probes/AbstractReceiverSweep.java` — the oracle-free assertion
`native-api/src/instantiable.rs` cites as `probes/W4Abstract.java` and which was
never in the tree. 29 sites across `java.nio.channels`, `java.nio.file`,
`java.nio.fs`, `java.lang.foreign` and the platform singletons; HotSpot 0
defects, this VM 1 in compatible mode and 7 under `--jdk-only`. The next lane
can now measure progress row by row instead of from one prose paragraph.

## Verification

Merged tree, release binary, same host.

```text
cargo test -p cratonvm-types                       591 passed, 0 failed  (see the flake note)
the seven native-builtins ratchets, both arms      PASS
cargo test -p cratonvm-native-builtins --lib       PASS
cargo test -p cratonvm-vm --lib                    PASS
SUITE=core                                          72 / 72
SUITE=all                                          112 / 112
CRATONVM_ARGS=--jdk-only                           111 / 112   (RJdkEnumerations)
probes/DodServiceLoaderSweep   compat and strict   IDENTICAL to HotSpot
probes/DodArrayStoreSweep      compat and strict   IDENTICAL to HotSpot
probes/AbstractReceiverSweep   compat 1, strict 7  the R4 rows above
the five definition-of-done arms, all three modes  unchanged
```

**A flake worth naming, seen three times.**
`cratonvm-types`' `arraylist_view::tests::a_fallback_mint_revokes_the_yield_permanently`
and `the_default_licenses_the_yield` failed under load on three separate runs
and **pass alone and in a clean full run** every time. They assert a
process-global latch, and `cargo test` runs the whole lib in one process, so a
sibling test that mints the fallback first decides their answer. It is test
isolation, not a VM defect, and it is not this lane's to move — but a gate that
is red one run in three is a gate people learn to ignore.

Two dev-tip reds were cleared in passing: a `known-issues/tomcat` page added by
`9cf97cc72` cited the internal tree by a prefixed path, which the doc-citation
gate refuses outside that tree (its own instruction is to drop the prefix), and,
earlier in this campaign, the `craton_gpu.rs` feature gates and the
`Class.forName` array CNFE.

## Where the probes are

`3b2901531` ("major doc consistency update before the realeas", 2026-08-29)
removed 915 files including the whole `probes/` tree, so every Reproduce block
in this directory now names files the tree does not have. Following the
convention `e1ff937b4` set for L6's sweeps, this lane's probes are named with
the commit that carries them and a one-line restore each:

```bash
git show 7da07b4ac:probes/DodSpringApp.java        > probes/DodSpringApp.java
git show 7da07b4ac:probes/DodJdbcWorkload.java     > probes/DodJdbcWorkload.java
git show 7da07b4ac:probes/DodJUnitRunner.java      > probes/DodJUnitRunner.java
git show 7da07b4ac:probes/DodH2JdbcSuite.java      > probes/DodH2JdbcSuite.java
git show 7da07b4ac:probes/DodServiceLoaderSweep.java > probes/DodServiceLoaderSweep.java
git show 7da07b4ac:probes/dodscreen-linux.sh       > probes/dodscreen-linux.sh
git show 7da07b4ac:probes/dod-arms.sh              > probes/dod-arms.sh
git show 7da07b4ac:probes/dod-report.py            > probes/dod-report.py
git show 7da07b4ac:probes/dod-summary.py           > probes/dod-summary.py
git show e8776985a:probes/DodArrayStoreSweep.java  > probes/DodArrayStoreSweep.java
git show e8776985a:probes/AbstractReceiverSweep.java > probes/AbstractReceiverSweep.java
chmod +x probes/dodscreen-linux.sh probes/dod-arms.sh
```

`DodServiceLoaderSweep` gained its `S-iteratorClass` row after `7da07b4ac`; take
that one from `e8776985a` too if the iterator-identity assertion is what you are
after.
