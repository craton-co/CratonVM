# The completion roadmap says FINAL and is stale: Phase 3 is closed, Phase 1 is five-ninths closed, and strict mode is now *better* than the default

**Status: 16 defects FIXED 2026-08-27, 4 recorded OPEN below.** Measured with five
probes against HotSpot 25.0.3+9, every one run in **both** modes.

## 1. Why re-adjudicate a document that says FINAL

`docs/feature-designs/jdk-only-completion-roadmap.md` is dated 2026-08-12 and
carries the word FINAL. Three of its four Phase 3 items already *looked* closed
in source, and its §5 states flatly that **"`--jdk-only-report` is a complete
census and nothing was using it"** — while `difftest/src/census.rs` and
`ledger.rs` consume exactly that file today.

A comment claiming a capability outlives the day it was true as often as one
denying it, so none of that reading was evidence. Two probes ask the VM.

## 2. PHASE 3 IS CLOSED — 35 rows, 0 differing lines, both modes

`probes/Phase3Sweep.java`. All four items, including the one the roadmap called
"the one live red in the suite":

| item | claim | measured |
| --- | --- | --- |
| **P3-A** | the JIT omits the `aastore` covariance check | `cold=ArrayStoreException hot=ArrayStoreException`, **cold == hot true**, and again for a covariant `Integer[]` seen as `Number[]` |
| **P3-B** | `MethodHandleProxies.asInterfaceInstance` → `ClassFormatError: ldc` | builds, calls, `isWrapperInstance` true |
| **P3-C** | typed linkage errors flattened to `ClassFormatError` | future version → `UnsupportedClassVersionError`; bad magic / truncated / empty → `ClassFormatError`; message carries no `Linkage(`, no brace, no `class_name:` |
| **P3-E** | `String.format` with no `Locale` localises against ROOT | `FORMAT=de_DE` → `1.234,50`, and `DISPLAY=US` does not disturb it |

P3-A was checked on the tier that matters: `store` is made hot with 400 000
legal stores before the illegal one is retried, so the second answer comes from
compiled code.

## 3. PHASE 1 — five of nine lanes closed, and the mechanism has INVERTED

`probes/Phase1Sweep.java`, all nine lanes, 80 rows. **Not one
`NoClassDefFoundError` anywhere.** The roadmap's mechanism — an essential native
survives strict mode, asks for a fabricated receiver, is correctly refused, and
kills its caller — no longer fires on any of the nine.

Closed: **P1-A** (`java.sql` loads, `SQLException` chains, its iterator walks),
**P1-B** (`System.getenv()` is a real `Collections$UnmodifiableMap`),
**P1-D** (`Consumer.andThen`, all of `Predicate`, `Function`, streams),
**P1-G** (`Hashtable`/`Vector`/`Collections.enumeration`),
**P1-I** (all six `Runtime.exec` overloads exit 7, and so does the
`ProcessBuilder.start` control the roadmap put beside them).

### The inversion, which is the finding

`--jdk-only` has **12** differing lines; **compatible mode has 18**. Every one of
the extra six is a place where strict is right and the default is wrong, because
strict declines to mint a fabricated class and gets the real JDK instead:

```text
P1-A updater class   HotSpot ..$AtomicReferenceFieldUpdaterImpl
                     --jdk-only  same          compatible  ..$RustJvmImpl
P1-I exec("")        HotSpot IllegalArgumentException
                     --jdk-only  same          compatible  ArrayIndexOutOfBounds
P1-I exec(null)      HotSpot NullPointerException
                     --jdk-only  same          compatible  IllegalArgumentException
```

`probes/AtomicUpdaterSweep.java` sharpens it to the point of embarrassment:
**87 rows, 0 differing lines under `--jdk-only`** — and compatible mode **dies**
at row 16 with `NoSuchMethodError: AtomicIntegerFieldUpdater$RustJvmImpl
.updateAndGet(Object, IntUnaryOperator)`.

## 4. FIXED — the fabricated updater had no superclass, so `instanceof` was false

`probes/RustJvmImplReach.java` was written to separate two hypotheses, because
every base method the registrar does *not* register happens to be lambda-taking,
and the sweep alone could not say whether inheritance was broken or those four
were special. It answered in one line:

```text
                              HotSpot                      CratonVM (compatible)
superclass                    AtomicIntegerFieldUpdater    java.lang.Object
u instanceof ..FieldUpdater   true                         FALSE
updateAndGet / getAndUpdate / accumulateAndGet / getAndAccumulate
                              ok                           NoSuchMethodError
```

`class_manager.rs`'s `jdk_superclass` **already maps all three
`$RustJvmImpl` names to their abstract base**, and its own comment says why:
*"the cast only succeeds if the returned object's class chain reaches the
abstract base"*. But `fabricate_class` picks the superclass from a hand-written
chain of four named special cases (`SSLSocketOutputStream`,
`SSLSocketInputStream`, `cratonvm/synthetic/Process`, `Proxy$Instance`) and
otherwise defaults to `java/lang/Object` — **it never asks that table.** The
mapping was dead code and the premise in its comment was false.

The asymmetry that hid it: the sibling `jdk_interfaces(name)` call thirty lines
below *is* consulted generally. One of the two tables was wired up and the other
was not, and nothing compared them.

**Fixed** by naming the three classes in the same idiom as the four cases above
them. Deliberately NOT by routing every fabricated stub through `jdk_superclass`:
that table covers a large slice of the JDK, and doing so would change
superclasses, field layouts, `instanceof` and catch-matching for classes this
change never measured. **The general divergence between the two tables is
recorded here and left to whoever owns classloading** — it is a real hazard, and
it should be closed by a decision rather than as a side effect of my probe.

Consequence beyond the four methods: `u instanceof AtomicIntegerFieldUpdater`
answered **false** for an object the VM had just handed back from
`newUpdater`, which is the exact failure the mapping was written to prevent.

### 4.1 The first fix was a SILENT NO-OP, for the same reason as the bug

Worth recording, because the failure mode repeated one level up. The first cut
copied the four cases above it verbatim:

```rust
self.get_loaded_class_id(base)
    .or_else(|| self.get_loaded_class_id("java/lang/Object"))
```

Rebuilt (29 minutes), re-probed: **superclass still `java.lang.Object`,
`instanceof` still false, all four methods still `NoSuchMethodError`.** Nothing
had changed, and nothing had failed either — the lookup simply missed and the
`or_else` quietly restored the very default the arm existed to override.

At the moment `newUpdater` mints the impl, the abstract base is **not a loaded
class**. `newUpdater` is itself a REGISTERED NATIVE, so the `invokestatic`
resolves through the native registry without the declaring class ever being
loaded from the image.

So the idiom I copied is only correct *by accident of boot order*: its four
classes (`java/io/OutputStream`, `java/lang/Process`, …) are loaded long before
anything that names them is minted. Copying a local idiom carries its unstated
preconditions with it. The fix is `load_class`, which is what the sibling
synthetic-stub path already uses for exactly this
(`Some(self.load_class(parent)?)`) — the same asymmetry as §4, one layer down.

**Only the probe caught it.** The build was clean, the check was clean, the arm
was demonstrably present in the source, and `CRATONVM_DBG_STUB_BT` confirmed the
code path ran. A fix can be present, reached, and inert.

## 5. FIXED — fifteen more, each in a family whose registrar's claim had drifted

The survey's standing prior is that **every defect so far sat in a family whose
registrar carried a stated justification that had drifted from its code**, never
in one that was merely thin. Three registrars still made that claim in as many
words. Two of the three paid out.

### 5.1 `getCanonicalHostName` answered the name it was given

`inet_address.rs` opens with *"`Inet6AddressImpl` — same surface, IPv6-flavoured"*
and `net_phase_e.rs` really does run one body for both concrete classes.
`InetFamilySweep` asked all 468 rows where v4 and v6 must part — address length,
any-local and loopback constants, IPv4-mapped collapse, scope ids, every `isMC*`
predicate — and **465 agreed**. The claim was right about nearly everything and
wrong about exactly what the prior pointed at: `getHostName` and
`getCanonicalHostName` shared `inet_addr_host_name_value`.

```text
getByAddress("h", 192.0.2.1).getCanonicalHostName().equals("h")
  HotSpot  false          CratonVM  true      (both modes)
```

Asked as a property so no resolver answer enters the diff: that row is false on
HotSpot for *every* resolver outcome — a real PTR name is not `"h"`, and neither
is `"192.0.2.1"` — and true exactly on a VM that routes both methods to one
body. `getCanonicalHostName` now does its own `ptr_lookup` (which uses
`NI_NAMEREQD`, so it fails rather than returning the numeric form) and falls back
to the textual address.

### 5.2 `getByAddress(null)` threw the wrong type

`UnknownHostException`, not `NullPointerException`: the JDK's two-argument form
is `if (addr != null) { .. }` then an unconditional throw, so null leaves by the
same door as a 5-byte array. `obj_arg` raises the NPE for all ~3900 of its call
sites, so the check had to go in the native.

### 5.3 `KeyStore.getCertificateAlias` was FATAL

`keystore.rs` says *"PKCS12 + JKS share the same `engine*` surface"*. The oracle
says they do not — JKS refuses a `SecretKey` entry and a null store password
where PKCS12 accepts both — but that is not what broke.

```text
KeyStore.getCertificateAlias(cert)
  HotSpot   "mixedcasealias"
  CratonVM  NullPointerException: because "this.keystore" is null
            at sun/security/util/KeyStoreDelegator.engineGetCertificateAlias
```

The module registers **sixteen** `engine*` methods and this was the seventeenth.
**An unregistered `engine*` in a half-shimmed class does not degrade — it reaches
real `KeyStoreDelegator` bytecode, which dereferences a field the natives never
populate.** The probe died at row 33 of 160, so 128 rows never ran. Now
registered, matching by DER rather than by mirror identity.

### 5.4 Aliases were neither lowercased nor null-checked

```text
setCertificateEntry("MixedCaseAlias", c); aliases()
  HotSpot  [mixedcasealias]      CratonVM  [MixedCaseAlias]
containsAlias("mixedcasealias")     HotSpot true   CratonVM false
containsAlias(null)                 HotSpot NPE    CratonVM no-throw
```

Both SPIs do `alias.toLowerCase(Locale.ENGLISH)` before touching their map, which
is where both behaviours come from at once — the null dereference and the case
fold. Twelve identical `.unwrap_or_default()` alias reads became one helper.
This is interop, not cosmetics: a store written by `keytool` and read here
disagreed about every alias that was not already lower case.

### 5.5 `SSLSocket.getOutputStream()` on an unconnected socket

```text
f.createSocket(); s.getOutputStream()
  HotSpot  SocketException      CratonVM  no-throw      (both modes)
```

The probe's own control — the same question on a plain `java.net.Socket` —
already agreed, which is what made it SSL-specific. Both SSL accessors now screen
on `new13_socket_ever_connected`, **the same predicate this file's `isConnected`
native already answered correctly on that very receiver** — so the information
was present and simply unread.

### 5.6 `getCreationDate` on an absent alias returned the epoch

```text
getCreationDate("nope")
  HotSpot   null      CratonVM  Wed Dec 31 21:00:00 UTC 1969
```

`.unwrap_or(0)` handed back a `Date` at time 0, so a caller testing
`getCreationDate(a) != null` to ask *does this entry exist* got true for every
alias in existence. The rendered value is the local-time face of epoch 0, which
is what made it read like a real timestamp rather than a default.

### 5.7 JKS accepted a null store password

```text
ks.store(out, null)
  JKS     HotSpot IllegalArgumentException   CratonVM no-throw
  PKCS12  HotSpot no-throw                   CratonVM no-throw
```

Another place the module's "PKCS12 + JKS share the same `engine*` surface"
header is false. Real `JavaKeyStore.engineStore` opens with
`if (password == null) throw new IllegalArgumentException`; `PKCS12KeyStore` has
no such line. Keyed on the receiver class, like the existing
`spi_supports_secret_keys` beside it — and note this is a DIFFERENT axis from
the format choice further down, which is decided by content.

### 5.8 `KeyStore.getDefaultType()` was the wrong case

`"PKCS12"` where the JDK ships `keystore.type=pkcs12`. `getDefaultType` is
specified as `Security.getProperty("keystore.type")` verbatim, so the table's
spelling *is* the method's answer.

### 5.9 The base-class natives shadowed a user subclass — and the crash was hiding it

Only visible AFTER §4 stopped compatible mode dying at row 16. The probe's whole
`[subclass]` section — the part written specifically to test the prior — had
never run.

`atomic_updater.rs` registered the accessors on the **abstract base** as well as
the impl, under the comment *"Also on the abstract base for virtual dispatch."*
`AtomicIntegerFieldUpdater` is a public abstract class with a protected
constructor, so an application may extend it and supply its own
`get`/`set`/`compareAndSet`. A native on the base runs in front of that
subclass's inherited bodies, so the JDK's base-class `getAndIncrement` —
specified in terms of `get` and `compareAndSet`, and therefore required to
dispatch back into the subclass — never did:

```text
[subclass] getAndIncrement result            HotSpot 100   CratonVM 10
[subclass] getAndIncrement entered subclass  HotSpot true  CratonVM FALSE
[subclass] getAndIncrement counters          gets=1 cas=1  gets=0 cas=0
[subclass] holder untouched                  HotSpot 10    CratonVM 56
```

The last row is the damaging one: the native read and wrote the caller's
**holder object** through its own slot layout, while the subclass's state — the
only state the subclass believes it has — sat untouched. Not an exception; a
silent write to the wrong object.

**Removed** — 16 base instance registrations plus the base half of two
`for cls in [IMPL, BASE]` loops. Safe rather than merely better, because
dispatch probes the receiver's own class first and every updater this module
hands out is a `$RustJvmImpl` carrying its own full registrations: the base rows
were already **dead for our own objects** and fired only for receivers we should
never have intercepted. `newUpdater` stays — it is a static factory with no
receiver.

`--jdk-only` was already clean here (87/87), because strict drops these bridges
and runs the JDK's own bytecode. **The fix makes the default agree with strict,
not the other way round** — which is the shape of this whole document.

Two of the sixteen were duplicate registrations of the same triple
(`get` and `compareAndSet` on the int base, registered twice), so the file also
had a last-write-wins ambiguity nobody was reading.

**The in-file test caught the edit, exactly as designed.**
`t19_h5_jdk_only_registers_nothing_from_this_module` asserts strict registers
nothing, and carries a per-triple **mutation control** asserting the same triples
ARE served in compatible mode — so a green strict assertion cannot come from a
typo. Removing the base rows made that control fail and say so; the list now
names the impl triples.

### 5.10 `newUpdater` never checked that the field is VOLATILE

```text
AtomicIntegerFieldUpdater.newUpdater(Holder.class, "plainInt")
  HotSpot  IllegalArgumentException      CratonVM  no-throw
```

`static` and `final` were both rejected, two checks above. Volatile — the third
of the JDK's three, and the only one whose absence is **silently unsafe rather
than merely wrong-typed** — was missing. The entire contract of a field updater
is that the field is volatile; an updater built over a plain field hands every
caller ordinary non-atomic reads and writes while looking exactly like an atomic
one.

Three in-module test fixtures declared their field with access flags `0`, so
they were relying on the missing check. Two of them are about vclass semantics
and were simply corrected to `ACC_VOLATILE`. **The third is worth naming**:
`t19_h5_aifu_new_updater_long_field_rejected` asserts only that an
`IllegalArgumentException` comes back — and "field must be volatile" is also an
`IllegalArgumentException`. With a non-volatile fixture it would have gone green
for a reason unrelated to the long/int descriptor split it is named for. A test
whose assertion is a bare exception TYPE cannot tell you which check fired.

### 5.11 An updater applied to the wrong class wrote an unrelated object

```text
u.get(null)                     HotSpot ClassCastException  CratonVM NullPointerException
rawUpdater.get(new Object())    HotSpot ClassCastException  CratonVM NO-THROW
```

The second row is the serious one. An `AtomicIntegerFieldUpdater<Holder>` cast
to a raw type and applied to some other class read and wrote **slot N of an
unrelated object** — whatever happens to occupy the offset `Holder.i` does.
Generics are erased, so the cast that makes this reachable is one an application
can perform by accident, and the JDK's runtime `isInstance` check is the only
thing between it and a silent cross-object write.

Both rows are one fix. The JDK's accessors open with `if (!tclass.isInstance(obj))
throw new ClassCastException()`, and `isInstance` is **false for null** — so a
null target leaves by the same door as a wrong-typed one and never reaches a
dereference. `require_target` was a single chokepoint used at 26 call sites,
which is why one edit covers the whole accessor surface.

The in-module test asserting NPE for a null target was asserting **this VM's
behaviour, not the JDK's**. Its name — `..._returns_npe_not_silent` — was right
about the intent (a null target must not be absorbed) and wrong about the type;
it is now `..._returns_cce_not_silent`, with the measurement in the body. A
caller catching `ClassCastException` around a field-updater access is catching
the documented type, and an NPE escaped it.

### 5.12 `newUpdater`'s four refusals all left by the wrong door

```text
newUpdater(Holder.class, "nope")                 HotSpot RuntimeException  CratonVM IllegalArgumentException
newUpdater(null, "i")                            HotSpot RuntimeException  CratonVM NullPointerException
newUpdater(Holder.class, null)                   HotSpot RuntimeException  CratonVM NullPointerException
ARFU.newUpdater(Holder.class, Integer.class,"i") HotSpot ClassCastException CratonVM IllegalArgumentException
```

All three factories share one body shape —
`try { tclass.getDeclaredField(name); .. } catch (Exception ex) { throw new
RuntimeException(ex); }` — so a null class, a null name and a missing field all
arrive as a plain `java.lang.RuntimeException`, never as the NPE or IAE that
caused them.

**Why correct this when IAE and NPE are themselves `RuntimeException`s, so a
`catch (RuntimeException)` is unaffected?** Because the difference runs the
other way. Application code that catches `IllegalArgumentException` around a
`newUpdater` call — a reasonable thing to write — catches *this VM's* refusal and
does **not** catch HotSpot's, so a recovery path that never runs on the
reference VM runs here. A wrong exception type is not only a wrong message; it
is a different set of catch clauses.

The fourth row is a separate door. `AtomicReferenceFieldUpdater` compares the
field's declared `Class` against the `vclass` argument, and an `int` field can
never equal a reference `vclass`, so it leaves as `ClassCastException`. The
existing CCE check was unreachable for exactly this case: it only ran for a
descriptor `ref_descriptor_to_internal_name` can name, which a primitive
descriptor is not, so the generic descriptor check upstream answered first.

**A third test asserting this VM's type rather than the JDK's**
(`..._missing_field_throws_iae`) failed on this change and is renamed with the
measurement in its body. That is three such tests in one family. The pattern is
worth naming: a test written from the implementation records what the code does,
and reads exactly like a test written from the specification.

## 6. OPEN, and deliberately not fixed here

* **`ConcurrentHashMap.elements()` never terminates**, both modes, no flags —
  dev's `a0168ed03`, bisected and recorded separately. This probe is what showed
  the defect does not need `--jdk-only-report`; that record is corrected.
* **`Arena`/`MemorySegment` report an impossible class identity.** Under
  `--jdk-only` an instance's `getClass().getName()` is
  `java.lang.foreign.MemorySegment` — the *interface*. `byteSize` answers 16
  correctly, so this is identity, not function; it is also the P1-E fabrication
  still standing, and it is `panama.rs`'s lane.
* **`AsynchronousFileChannel.write` returns a `CompletableFuture`** where HotSpot
  returns `sun.nio.ch.PendingFuture`. Every value agrees; the type does not.
* **`KeyStore.getInstance("JCEKS")` throws `KeyStoreException`.** The type is
  simply not offered, which costs 44 of the probe's 160 rows. Unlike everything
  above this is a missing capability rather than a wrong answer, and it is a
  provider-registration question (`jca/provider_chain.rs`) rather than a
  keystore one.

## 6.5 The three arms, and the two reds that are not this branch's

Run on the merged tree at `646329f81`, release binary, host otherwise idle:

| arm | result |
| --- | --- |
| `CRATONVM_ARGS=--jdk-only` | 111 passed, 1 failed — `RJdkEnumerations` |
| `SUITE=all` | 110 passed, 2 failed — `RJdkEnumerations`, `RBlockingQueue` |
| `SUITE=core` | **72 passed, 0 failed** |

**`RJdkEnumerations`** is dev's `a0168ed03`, bisected in two builds and recorded
at `rjdkenumerations-is-red-on-dev-from-the-chm-values-cursor-20260827.md`.
Reverting that commit alone, with this branch's fixes still in, is clean 3/3.

**`RBlockingQueue` is a KNOWN FLAKE, and I checked rather than assumed.** It is
new relative to this morning's arms, and the window between contains both this
branch's 16 fixes and 70 dev commits, so "probably flaky" was not good enough.
Measured on this binary: **3/3 pass standalone** through the same harness, and
**pass on a repeat `SUITE=all`** — 4 passes against the 1 failure. A change of
mine that broke it would not pass 4 of 5.

Then the part that actually settled it: `docs/known-issues/jdk-only/
HANDOFF-20260812.md` already says, in bold, *"`RBlockingQueue` is a SUSPECTED
FLAKE, not a regression — do not chase it"*, with the same signature recorded
two weeks earlier — one failure in a loaded full-suite run, six consecutive
passes after. It is a heavy concurrency vector, and that page names it as one of
the vectors on this host that only fail under suite load.

**Reading the existing records before starting the bisect would have saved the
repeat run.** The instinct to attribute a new red before landing was right; the
order was wrong. Search the known-issues tree for the vector name FIRST — it
costs seconds, and this project's convention is that such a page exists.

Nothing in the three arms is attributable to this branch.

## 7. The prior's score, stated because it is the method

Three registrars claimed a shared surface. **All three paid**: Inet 2 defects,
KeyStore 5, AtomicUpdater 2 — and the `on both impl + base` registration was
precisely the one the prior was aimed at (§5.9).

**I got that wrong in the first draft of this page**, and the reason is worth
more than the score. The first run had compatible mode dying at row 16, so the
probe's entire `[subclass]` section — the part written to test the prior — never
executed. I read "no differing rows there" off a run that had not reached them
and wrote that the prior had missed. **A crash partway through a probe is not a
result for the rows after it**, and a diff tool will happily not mention them.

Guard against it the way this page's other numbers are guarded: check the row
COUNT before reading the diff. 116 produced against 160 expected is the finding,
and it was visible on the first line of output.

## Reproduce

```bash
for p in Phase1Sweep Phase3Sweep AtomicUpdaterSweep InetFamilySweep KeyStoreFamilySweep RustJvmImplReach; do cratonvm --java-home "$JDK" --jdk-only -cp probes/out $p; done
```

Run each without `--jdk-only` too: on this batch the two modes disagreed with
each other more often than strict disagreed with HotSpot.
