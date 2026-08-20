# C19-1 — `RJdkOptionalShape`: the executable form of C12-3, and an honest account of which rows reach it

**2026-08-13, lane C19.** C12-3 diagnosed the defect and named the run that
settles it; nothing had been written. This record lands
`regression-suite/src/RJdkOptionalShape.java` — **1,416 checks, PASS on HotSpot
25.0.3+9-LTS, mutation-checked in two directions** — and states plainly which
of its blocks can reach the diagnosed natives and which cannot.

**This lane could not build or run the VM.** Every number below was executed
against HotSpot 25.0.3+9-LTS on this host. Nothing about CratonVM's actual
behaviour is asserted here; the fixture is the instrument, and its first
CratonVM run is ahead.

---

## 1. The one fact C12-3 did not have, and it sharpens the diagnosis

C12-3 says the natives "model an `Optional` as *(flag, payload)* and the real
class models it as *(reference-or-null)*". That is right, and there is a reason
the idiom looked correct to whoever wrote it. `javap -p --module java.base`,
JDK 25:

```
java.util.Optional         private final T value;              <- ONE slot, a REFERENCE
java.util.OptionalInt      private final boolean isPresent;    <- slot 0
                           private final int value;            <- slot 1
java.util.OptionalLong     private final boolean isPresent; private final long value;
java.util.OptionalDouble   private final boolean isPresent; private final double value;
```

**The (flag, payload) layout is the real layout of the three PRIMITIVE
Optionals.** The nine sites applied the primitive family's layout to the
reference one. That is not a detail: it means a fixture built on `OptionalInt`
would read green on exactly the class that is broken, and it is why the
fixture's `prim` block exists as a distinct family rather than being folded into
`core`.

## 2. What the fixture is, and how it is driven

Seven families, run in ascending order of how likely each is to abort the VM
rather than fail an assertion (an `Int` in a reference slot reaches `getClass()`
and `toString()`), so a VM that dies in `httpmint` has already reported whether
the class library itself is sound. `--only=<family>` runs one; `--list` prints
the names. House harness style throughout: `check`/`sectionEnd`, a per-family
`CK RJdkOptionalShape <family>=<n>`, and a final
`PASS RJdkOptionalShape (N checks)`.

| family | checks | what it is |
|---|---|---|
| `core` | 123 | full `java.util.Optional` contract on Java-constructed values. **NEGATIVE CONTROL** |
| `prim` | 100 | `OptionalInt`/`Long`/`Double`, whose layouts differ. **NEGATIVE CONTROL** |
| `stream` | 363 | stream terminal ops that mint an Optional |
| `version` | 262 | `Runtime.Version` accessors |
| `misc` | 130 | `ModuleDescriptor`, `describeConstable`, `StackWalker` |
| `process` | 172 | `ProcessHandle` and `ProcessHandle.Info` |
| `httpmint` | 266 | **the rows that reach C12-3's natives** |

### `laws()` — twenty-one propositions, in a FIXED number of checks

Most of the interesting routes are host-dependent in their PRESENCE
(`info().command()` is present on some platforms and absent on others) but not
in their CONTRACT. `laws(site, optional, elementType)` asserts the twenty-one
things true of every `Optional` — `isPresent()`/`isEmpty()` disagree; `get()`
either returns the value or throws `NoSuchElementException`; `orElse`,
`orElseGet`, `orElseThrow`, `orElseThrow(supplier)`, `map`, `flatMap`,
`filter`, `ifPresent`, `ifPresentOrElse`, `equals`, `hashCode`, `toString`,
`stream`, `or` all agree with it; and the function-arity laws (`map`'s function
runs exactly `present ? 1 : 0` times) — with the present and absent arms
asserting DIFFERENT PROPOSITIONS rather than different NUMBERS of them. The
published check count is therefore not a host fact.

**The sharpest law is the type check**, and it is the one that makes the
host-dependent rows worth having: a present `Optional` must `get()` an instance
of its DECLARED element type. `Int(1)` is not a `Duration`, not an `Instant`
and not a `ProcessHandle`, so the type law fires on every present row whatever
the value would have been.

## 3. WHICH ROWS ACTUALLY REACH THE DEFECT — the honest answer

The parent lane asked for this explicitly and it is the most important section.

### Reaches it, verified by construction (`httpmint`)

C12-3's table names nine sites. **Four of them answer off a builder with no
network, no DNS and no TLS peer**, and the fixture drives all four:

| C12-3 site | call | absent row | present row |
|---|---|---|---|
| `http2.rs:1289` | `HttpClient.connectTimeout()` | `newBuilder().build()` | `.connectTimeout(1500 ms)` |
| `http2.rs:1727` | `HttpRequest.timeout()` | `newBuilder(uri).GET()` | `.timeout(7000 ms)` |
| `http2.rs:1750` | `HttpRequest.version()` | `newBuilder(uri).GET()` | `.version(HTTP_2)` |
| `http2.rs:1706` | `HttpRequest.bodyPublisher()` | a GET | `.POST(ofString("hi"))` |

Each is asked twice. The **absent** row catches the inverted `isPresent()`
directly (`Int(0)` is not null to `ref_operand_is_null`). The **present** row
catches `get()` returning the flag, and it does so by DEREFERENCING the value —
`Duration.toMillis()` must be 7000, `HttpClient.Version.name()` must be
`HTTP_2` and be `==` the enum constant, `BodyPublisher.contentLength()` must be
2 — because C12-3's own prediction is that the caller gets "an Int where every
caller dereferences an object".

This is the same shape as C12-3's own §"How to settle it" probe, widened from
one call to four and from `println` to assertions.

### Cannot reach it (stated, not papered over)

`sslSession()` (`:2121`), `previousResponse()` (`:2108`) and `:2209` all hang
off an `HttpResponse`, which needs a real response. **No row in this fixture
touches them.** `previousResponse()` is the row C12-3 calls the one that
"settles what this is" (right arity, wrong type) and it is also the row the
layout-alias instrument is blind to — so it remains uncovered by both the
instrument and this fixture. That is a real gap and it needs either a loopback
`HttpServer` fixture or a Rust-side unit test.

### May reach it; NOT established by this file

`stream`, `version`, `process` and `misc` exercise surfaces that RETURN an
`Optional` and are plausible candidates to be minted Rust-side on this VM. The
fixture says which route it expects to mint each, in a comment beside each
block:

* **`process` — the strongest of the four.** Every value is a syscall answer,
  so the `Optional` wrapping it is built where the syscall is; W7-10 records
  `ProcessHandle`'s interface STUB BODIES specifically, which is the shape that
  produces an `Optional` with nothing in it.
* **`stream` — second.** W7-2 records the primitive-stream terminal surface as
  a place where terminal ops are Rust-side. `IntStream.max()` returning an
  `OptionalInt` would be minted there.
* **`misc` — `ModuleDescriptor` first.** W2-3 records the descriptor answering
  fabricated empty sets, so it is served by the VM.
* **`version` — weakest of the four.** `Runtime.version()`'s numbers are the
  VM's own, so its accessors are natural candidates, but nothing in the record
  set says they are.

**A green run of these four families proves the CONTRACT holds on those routes.
It does not prove the minting path is sound, because it does not establish that
those routes mint anything.** If C19's fixtures are used to argue C12-3 is
closed, the argument must rest on `httpmint` and on `httpmint` alone.

### Proves nothing about the defect, by design

`core` and `prim` build their Optionals in Java, so the real
`java.util.Optional` bytecode builds them and C12-3 cannot reach them. They are
contract coverage and a NEGATIVE CONTROL: if they are red, the defect is in
`java.util.Optional` itself or in the interpreter, and `httpmint` would then be
reporting a symptom of something else.

`core` does carry one row that is load-bearing for this defect's arithmetic:
**`Optional.of(Integer.valueOf(0))` must be PRESENT**. The absent flag is
`Int(0)` and a genuinely present `Optional` holding boxed zero has the same slot
content by value — same bits, opposite answers. A VM that stores an int flag in
the value slot cannot tell them apart, and the fixture states the pair
explicitly so nobody later "simplifies" one of them away.

## 4. The HotSpot transcript (the oracle)

```
$ javac -d out regression-suite/src/RJdkOptionalShape.java
$ java -cp out RJdkOptionalShape
CK RJdkOptionalShape core-of-zero-present=1
CK RJdkOptionalShape core-empty-present=0
CK RJdkOptionalShape core=123
CK RJdkOptionalShape prim-int-empty-present=0
CK RJdkOptionalShape prim-int-zero-present=1
CK RJdkOptionalShape prim=100
CK RJdkOptionalShape stream-empty-findFirst-present=0
CK RJdkOptionalShape stream-int-empty-max-present=0
CK RJdkOptionalShape stream=363
CK RJdkOptionalShape version-plain-build-present=0
CK RJdkOptionalShape version-lts-build=12
CK RJdkOptionalShape version=262
CK RJdkOptionalShape misc-module-mainClass-present=0
CK RJdkOptionalShape misc=130
CK RJdkOptionalShape process-self-by-pid-present=1
CK RJdkOptionalShape process=172
CK RJdkOptionalShape mint-connectTimeout-absent-present=0
CK RJdkOptionalShape mint-timeout-absent-present=0
CK RJdkOptionalShape mint-version-absent-present=0
CK RJdkOptionalShape mint-bodyPublisher-absent-present=0
CK RJdkOptionalShape mint-connectTimeout-present-millis=1500
CK RJdkOptionalShape mint-timeout-present-millis=7000
CK RJdkOptionalShape mint-version-present-name=HTTP_2
CK RJdkOptionalShape mint-bodyPublisher-present-len=2
CK RJdkOptionalShape mint-client-version-direct=HTTP_2
CK RJdkOptionalShape httpmint=266
CK RJdkOptionalShape checks=1416
PASS RJdkOptionalShape (1416 checks)
```

**The four `mint-*-absent-present=0` lines are the diagnosis, printed.** They
are `CK` observables, so they survive `extract()` and reach the cross-VM diff:
a CratonVM that prints `=1` there fails the diff even in the branch where its
assertion somehow does not throw.

Byte-identical over three consecutive runs. No line on a non-`PASS`/`CK`
prefix, so guard G1 is clean. Run against `harness-guard.sh`'s own functions:

```
RJdkOptionalShape: oracle_guard=0 extract_guard=0 count=1416 lines=28
```

G1/G2/G3/G4 all clean, and the count parses in both accepted spellings. The
class must NOT be added to `harness-uncounted.txt` — the ratchet runs in both
directions.

## 5. MUTATION CHECK — two mutants, because the defect has two halves

Both mutants replace exactly one seam. `opt()` / `optI()` / `optL()` / `optD()`
are identity functions through which every `Optional` in the file passes, so a
mutant differs from the fixture by one method body per flavour and cannot
accidentally test something else.

### Mutant O1 — the full defect: the value slot holds the int flag

```java
static <T> Optional<T> opt(Optional<T> o) {
    @SuppressWarnings("unchecked")
    Optional<T> flagged = (Optional<T>) Optional.of(Integer.valueOf(o.isPresent() ? 1 : 0));
    return flagged;
}
```

```
=== mutO ===
  core : RED   core.empty: get() on a PRESENT Optional must return a java.lang.String, got java.lang.Integer (threw none) -- an int presence flag in the value slot is what this looks like
  prim : RED   prim.int.empty: isPresent() must be false
  stream : RED   stream.findFirst.empty: get() on a PRESENT Optional must return a java.lang.String, got java.lang.Integer (threw none) ...
  version : RED   version.parse-lts.optional: get() on a PRESENT Optional must return a java.lang.String, got java.lang.Integer (threw none) ...
  misc : RED   misc.module.mainClass: get() on a PRESENT Optional must return a java.lang.String, got java.lang.Integer (threw none) ...
  process : RED   process.parent: get() on a PRESENT Optional must return a java.lang.ProcessHandle, got java.lang.Integer (threw none) ...
  httpmint : RED   http.client.connectTimeout.absent: get() on a PRESENT Optional must return a java.time.Duration, got java.lang.Integer (threw none) ...
```

**All seven families red, each naming its own failing row.**

### Mutant O2 — the weaker form C12-3 names as its own fallback

C12-3 §"How to settle it" says: *"If CratonVM instead prints `isPresent=false`,
then `ifnull` treats `Int(0)` as null somewhere on this path and §3's first row
is wrong — in which case the remaining rows (a flag of 1 being returned by
`get()`) still stand."* That branch is a real possibility and it deserves its
own mutant, because a fixture that only detects the strong form would report
green on the weak one.

```java
static <T> Optional<T> opt(Optional<T> o) {
    if (!o.isPresent()) { return o; }          // the empty case accidentally right
    @SuppressWarnings("unchecked")
    Optional<T> flagged = (Optional<T>) Optional.of(Integer.valueOf(1));
    return flagged;
}
```

```
=== mutO2 ===
  core : RED   core.of: get() on a PRESENT Optional must return a java.lang.String, got java.lang.Integer (threw none) ...
  prim : GREEN
  stream : RED   Stream.of(7, 8).findFirst() must be 7 on a SEQUENTIAL stream
  version : RED   version.parse-lts.optional: get() on a PRESENT Optional must return a java.lang.String, got java.lang.Integer (threw none) ...
  misc : RED   misc.module.rawVersion: get() on a PRESENT Optional must return a java.lang.String, got java.lang.Integer (threw none) ...
  process : RED   process.parent: get() on a PRESENT Optional must return a java.lang.ProcessHandle, got java.lang.Integer (threw none) ...
  httpmint : RED   http.client.connectTimeout.present: get() on a PRESENT Optional must return a java.time.Duration, got java.lang.Integer (threw none) ...
```

**`prim` GREEN here is CORRECT and is itself a control**, not a hole: O2 mutates
only `opt()`, and the three primitive seams are untouched. A `prim` that went
red would mean the block was reading state it does not own.

## NOMINATION — `regression-suite/run.sh` (this lane may not edit it)

`RJdkOptionalShape` belongs in **`CORE_CLASSES`**, not `JDKONLY_CLASSES`.
`http2.rs` is registered in both arms and `java.util.Optional` is a real JDK
class either way, so this is a default-mode compatibility concern — the same
reasoning `run.sh` already records for `RJdkViews`, whose `RJdk` prefix is
likewise a naming convention only. It is deliberately not in both lists: under
`CRATONVM_ARGS="--jdk-only"` every scheduled class already receives the flag,
so a second registration would run the identical command twice.

REPLACE (end of the `CORE_CLASSES` line, `run.sh:106`):

```
RImmutableFactoryTypes RJdkStringCodePoints"
```

WITH:

```
RImmutableFactoryTypes RJdkStringCodePoints RJdkOptionalShape"
```

Without this line the vector compiles and never runs, and `run.sh`'s own list
hygiene flags it as an unlisted `src/*.java` — that report is the backstop, not
a substitute.

## Residuals

1. **Three of the nine sites are unreachable from this fixture** and one of
   them (`previousResponse()`) is also invisible to the layout-alias
   instrument. Nothing in this repository currently covers them. A loopback
   `com.sun.net.httpserver.HttpServer` fixture would reach `sslSession()` only
   with TLS; a plain-HTTP one reaches `previousResponse()` via a 302. That is
   the natural follow-up and this lane did not write it.
2. **Whether `stream` / `version` / `process` / `misc` mint Rust-side is
   unestablished.** Each block's comment says which route is EXPECTED to; none
   of them says it does. Settling this costs one
   `--dump-native-registry` grep per family and it was not run here.
3. **`process` asserts no PRESENCE except one.** `info().command()`,
   `user()`, `startInstant()`, `totalCpuDuration()`, `commandLine()` and
   `arguments()` are platform facts and get laws only. The single presence
   assertion is `ProcessHandle.of(self.pid())`, which must be present because
   this process is alive, and it is checked three ways (present, `equals`
   `current()`, same `pid()`) precisely because it is the only one available.
4. **The fixture was validated on Windows.** The Linux run may take different
   presence branches in `process` and `misc`; the laws are written so that
   costs no check-count movement, but that claim has not been executed on
   Linux.
5. **This record asserts nothing about CratonVM.** The fixture's first
   CratonVM run is ahead. A red in `httpmint` is the gate working; a red in
   `core` or `prim` means something else is wrong and the `httpmint` result
   should not be read until it is fixed.
