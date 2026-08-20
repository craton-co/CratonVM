# F25-1 — the four doors the null session never knocked on, F21's nineteen landed, and the argument-kind vector that only exists when two arguments are bad at once

**Status: FIXTURES ONLY. Nothing in `native-*/src/` was touched.** Three
regression fixtures and this directory's index. Every expected value below was
**MEASURED on Microsoft OpenJDK 25.0.3+9-LTS** (`Microsoft-13877124`) on this
host before it was written, and **every row added was mutation-checked**: each
expected value was replaced by what a *named* wrong implementation answers and
the fixture re-run. **86 of 86 mutants died. None survived.**

**Prov: HotSpot column MEAS (this host, `scratchpad/f25/`). CratonVM column NOT
MEASURED AT ALL** — this lane was forbidden to build or run CratonVM, and no
row below carries a CratonVM number, predicted or otherwise. What the fixtures
will report on CratonVM is unknown to this lane; §6 names the rows most likely
to be the first to go red and why.

| file | why it is mine |
|---|---|
| `regression-suite/src/RJdkIntrinsics2.java` | assigned |
| `regression-suite/src/RSslNullSession.java` | assigned |
| `regression-suite/src/RJdkSecurity.java` | assigned |
| `docs/known-issues/jdk-only/INDEX.md` | assigned |
| this document | assigned (new `.md` under `docs/known-issues/jdk-only/`) |

---

## 0. The denominators, in one table

Say these out loud in any handoff. A silent denominator change reads as a pass.

| class | family | before | after | delta |
|---|---|---|---|---|
| `RJdkIntrinsics2` | `bounds` (`sectionEnd("bounds", …)`) | **102** | **121** | +19 |
| `RJdkIntrinsics2` | class total | 1003 | **1022** | +19 |
| `RSslNullSession` | `nullSession()` per door ×3 doors | — | — | +15 |
| `RSslNullSession` | new `invalidate` arm, 8 rows × 2 doors | — | — | +16 |
| `RSslNullSession` | new `attributes` arm | — | — | +11 |
| `RSslNullSession` | class total | **47** | **89** | +42 |
| `RJdkSecurity` | new `srArgKinds` family (**new tripwire**) | — | **43** | +43 |
| `RJdkSecurity` | class total | **80** | **123** | +43 |

`RJdkIntrinsics2`'s other families are untouched. In particular `hex` reads
**77** here and not 73 — that move is F2-1's, already in the working tree when
this lane opened, not this lane's.

Every one of these numbers was produced by running the fixture, never by adding
on paper:

```text
CK RJdkIntrinsics2 hex=77
CK RJdkIntrinsics2 bounds=121
CK RJdkIntrinsics2 checks=1022
PASS RJdkIntrinsics2 (1022 checks)

CK RSslNullSession failures=0
CK RSslNullSession checks=89
PASS RSslNullSession (89 checks)

CK RJdkSecurity srArgKinds=43
CK RJdkSecurity checks=123
PASS RJdkSecurity (123 checks)
```

---

## 1. F21-1 N1 landed — GAP 3c, nineteen rows, `bounds` 102 → 121

F21-1 §9 measured read-only contagion across seven buffer families with a
916-row probe and wrote nineteen fixture rows it could not land, because
`regression-suite/src/` was not its file. **They are not pasted on trust.**
This lane re-ran them: spliced at F21's named anchor, `javac` clean, no
local-name collision (`roDup`, `roBb`, `wArr`, `wIb` are fresh; `t`, `sink`,
`OPAQUE_I`, `check`, `step`, `nameOf` are the file's), no new `import`
(`ByteBuffer`/`CharBuffer`/`IntBuffer` are already at lines 1/3/4), and the
family runs **121**.

The contract encoded, MEASURED, all seven families identical:

| source | `duplicate()` | `slice()` | `slice(int,int)` | `asReadOnlyBuffer()` |
|---|---|---|---|---|
| writable | false | false | false | **true** |
| read-only | **true** | **true** | **true** | **true** |

There is no composition of `java.nio` buffer operations that returns to
writable. There is no `asWritableBuffer`.

### 1.1 Why the rows are appended and not inserted

**Deliberate, and it is the second time.** This file has a prediction table
elsewhere keyed by **check NUMBER** — `hex` is quoted as "stops at 32 of 73"
with rows 32–49, 54–57, 69 and 73 named as flipping. F2-1's four `hex` rows
were appended at the END of that family for exactly this reason, and its
comment says so. GAP 3c does the same: the nineteen go **after** GAP 3b and
before `sectionEnd`, so they are `bounds` checks **103–121** and every earlier
number in `bounds` still means what it meant.

### 1.2 The supertype trap, and why every row compares an exact class name

`java.nio.ReadOnlyBufferException` **extends**
`java.lang.UnsupportedOperationException`
(`jdk25src/java.base/java/nio/ReadOnlyBufferException.java:40`). An
`instanceof`-shaped assertion therefore discriminates in **one direction
only**: it accepts a `ReadOnlyBufferException` where `UnsupportedOperation` was
meant and cannot see the swap. Every row uses `nameOf(t)` and compares the
exact name.

This is not theoretical. Mutants **M16**, **M17** and **M18** are that exact
swap in both directions, and all three die only because the comparison is by
name. Under an `instanceof` spelling, M16 and M17 would have **survived**.

### 1.3 Mutation results — 19 of 19 died

Each mutation replaces the expected value with what a *specific* wrong
implementation answers; each was compiled and run with `--only=bounds`.

| # | row | mutated to (the wrong implementation it names) | |
|---|---|---|---|
| M1 | `roDup.isReadOnly()` | `false` — a `duplicate` that copies pos/lim/cap and writes nothing to `isReadOnly` | DIED |
| M2 | `!roDup.hasArray()` | `true` — a storage-only heap-or-direct classifier | DIED |
| M3 | `roDup.array()` ROBE | `none` — the pre-fix `native_bb_duplicate`: **no throwable at all** | DIED |
| M4 | `roArr.slice().isReadOnly()` | `false` — the pre-fix `native_bb_slice` | DIED |
| M5 | `roArr.slice(0,2).isReadOnly()` | `false` — the absolute overload is a separate registration | DIED |
| M6 | `roDup.asReadOnlyBuffer().isReadOnly()` | `false` | DIED |
| M7 | `!wArr.duplicate().isReadOnly()` | `true` — an implementation that stamps EVERY derived view read-only, which passes M1–M6 | DIED |
| M8 | `wArr.duplicate().hasArray()` | `false` | DIED |
| M9 | `roBb.duplicate().isReadOnly()` | `false` | DIED |
| M10 | `!roBb.duplicate().hasArray()` | `true` | DIED |
| M11 | `roBb.duplicate().array()` ROBE | `none` | DIED |
| M12 | `roBb.slice().isReadOnly()` | `false` | DIED |
| M13 | `roBb.slice().arrayOffset()` ROBE | `none` | DIED |
| M14 | `!hbb.duplicate().isReadOnly()` | `true` | DIED |
| M15 | `wIb.arrayOffset() == 0` | `== 4` — a body answering capacity | DIED |
| M16 | typed r/o `arrayOffset()` ROBE | `UnsupportedOperationException` — **the supertype swap** | DIED |
| M17 | typed r/o **duplicate** `arrayOffset()` ROBE | `UnsupportedOperationException` — the supertype swap through the contagion | DIED |
| M18 | writable typed view `arrayOffset()` UOE | `ReadOnlyBufferException` — a **read-only-first** implementation | DIED |
| M19 | that UOE's `getMessage()` is null | `"direct buffer has no backing array"` — the literal that survived a prior repair | DIED |

F14-1 §6.2's warning still applies and is not fixed by anything here:
`bounds` aborts at its first failure, so on CratonVM none of rows 103–121
executes until 1–102 pass.

---

## 2. `RSslNullSession` — F18-1's four doors, 47 → 89

### 2.1 What F18 left

F18-1 §7 registered `invalidate()`, `getPeerHost()`, `getPeerPort()` and
`getSessionContext()` on `javax/net/ssl/SSLSession` for real-JDK mode. Before
that they had **no registration at all** — which is not a wrong value but an
`AbstractMethodError` off the `Code`-less interface declaration. F18's own
finding about this fixture:

> The fixture asserts nothing about `invalidate`, `getPeerHost`, `getPeerPort`
> or `getSessionContext` … a fixture extended with `s.invalidate()` or
> `s.getPeerHost()` would today abort DOOR 1 … exactly the way it aborted on
> `getHandshakeSession` before E31 registered it.

Same shape as the `getHandshakeSession` gap E31-1 found: **0 of 47 checks was
correct arithmetic and a coverage hole at the same time.**

### 2.2 Measured, both doors identical (unconnected `SSLSocket`, pre-handshake `SSLEngine`)

```text
getPeerHost()           = null
getPeerPort()           = -1
getSessionContext()     = null
getPeerCertificates()   SSLPeerUnverifiedException: peer not authenticated
getPeerPrincipal()      SSLPeerUnverifiedException: peer not authenticated
invalidate()            returns normally; after it:
    isValid()           = false          (unchanged)
    getSessionContext() = null           (unchanged)
    getId()             = byte[0], byte-for-byte identical
    getCipherSuite()    = SSL_NULL_WITH_NULL_NULL   (unchanged)
    getProtocol()       = NONE                      (unchanged)
    getPeerHost/Port    = null / -1                 (unchanged)
invalidate() twice      idempotent, still returns normally
```

Three facts in there are worth their own line.

**(a) `getPeerPort()` is `-1`, and `0` is not a smaller version of that.**
`Int(0)` in a port slot means *unwritten*. Both wide producers write `-1`
explicitly and `http2.rs`'s 6-field session writes no slot at all, so a `0` is
the allocator's fill being reported as an answer. A connected peer never
reports port 0, which is what makes `-1` safe and `0` the plausible-wrong-value
this directory keeps recording as worse than a loud one.

**(b) An engine that HAS a peer host still has a session that does not.**
`ctx.createSSLEngine("localhost", 443)` answers `getPeerHost() == "localhost"`
from the **engine**, and that same engine's **session** answers `null`. So an
implementation that forwarded the engine's own peer host into the session
passes on the no-argument door and fails on the two-argument one. This is
recorded because it is the obvious "improvement" someone will make.

**(c) The refusal message is `peer not authenticated`, on BOTH doors.** The
fixture asserted the exception *class* and nothing about the message. The
converse of this directory's usual warning applies: an `IOException` whose
message names the right thing never matches the caller's `catch`, and the right
class carrying some other explanation is what lets a wrong internal cause
survive a repair.

### 2.3 The attribute map must not shadow the identity doors

New arm, and the reason it exists is E31-1 §2: **slot 3 is the peer host on the
6- and 8-field session shapes and the ATTRIBUTE MAP on the 4-field one.** A
width-blind read returns a `java.util.HashMap` through a
`()Ljava/lang/String;` descriptor **as soon as anything has called
`putValue`** — and Jetty's `SecureRequestCustomizer.retrieveSni()` does, on
every SSL request. Nothing in this suite armed that trap.

The arm arms it: `putValue("cratonvm.f25", "v")`, then asks `getPeerHost()`,
`getPeerPort()` and `getSessionContext()` again. It is written as a state
**change**, not a state read — six rows first establish that the attribute
really landed (`getValue` round-trips, its class is `java.lang.String`,
`getValueNames()` names it), because without them a VM whose `putValue`
silently did nothing would pass the three shadow rows for the wrong reason.

Measured on HotSpot: `putValue`, `getValue`, `getValueNames` and `removeValue`
all behave, and the three identity doors are **unchanged** — `null`, `-1`,
`null`.

### 2.4 Mutation results — 24 of 24 died

| # | row | mutated to | |
|---|---|---|---|
| S1 | `door.getPeerHost` `null` | `"localhost"` — the engine's peer host forwarded into the session | DIED |
| S2 | `door.getPeerPort` `-1` | `0` — an unwritten slot reported as an answer | DIED |
| S3 | `door.getSessionContext` `null` | `sun.security.ssl.SSLSessionContextImpl` — a fabricated context | DIED |
| S4 | `getPeerCertificates` message | `null` — a refusal built with the no-argument constructor | DIED |
| S5 | `getPeerPrincipal` message | `null` | DIED |
| S6 | `invalidate()` returns | `AbstractMethodError` — **the pre-F18 state** | DIED |
| S7 | `isValid()` after | `true` — an inverted bit | DIED |
| S8 | `getSessionContext()` after | fabricated | DIED |
| S9 | `getId().length` after | `32` — the fabricated id E12-1 found | DIED |
| S10 | `getCipherSuite()` after | `TLS_AES_256_GCM_SHA384` — E12-1's fabrication | DIED |
| S11 | `getPeerPort()` after | `0` | DIED |
| S12 | second `invalidate()` | `IllegalStateException` — not idempotent | DIED |
| S13 | `isValid()` after the second | `true` | DIED |
| S14 | `attrs.shadow.getPeerHost` | `{cratonvm.f25=v}` — **E31-1 §2's HashMap through a String descriptor** | DIED |
| S15 | `attrs.shadow.getPeerPort` | `0` | DIED |
| S16 | `attrs.shadow.getSessionContext` | fabricated | DIED |
| S17 | `attrs.after.getValue` | `null` — a `putValue` that silently no-ops | DIED |
| S18 | `attrs.after.valueNames` | `[]` | DIED |
| S19 | `attrs.after.getValue.class` | `java.util.HashMap` | DIED |
| S20 | `attrs.removed.valueNames` | `[cratonvm.f25]` — a `removeValue` that no-ops | DIED |
| S21 | `attrs.putValue.raises` | `AbstractMethodError` | DIED |
| S22 | `attrs.before.valueNames` | `[cratonvm.f25]` | DIED |
| S23 | `attrs.before.getValue` | `"v"` | DIED |
| S24 | `attrs.removeValue.raises` | `AbstractMethodError` | DIED |

### 2.5 What this fixture CANNOT prove, and why no row pretends to

**It opens no HTTPS connection.** Every arm uses an object that was never
connected: nothing binds, listens, resolves or dials, so the vector stays safe
in a sandboxed or offline CI and cannot flake on a port. That property is the
file's own header contract and this lane did not break it.

The consequence has to be said plainly, because F18's headline is a two-part
fact and only one part is reachable here:

* **F18's finding is that `invalidate()` moves TWO accessors** — `isValid()` to
  false **and** `getSessionContext()` to null — correcting an "isValid and
  nothing else" claim repeated in four comments across three files. The second
  half is only visible where a live `SSLSessionContextImpl` exists. This
  fixture asserts the other side of the same contract: that `invalidate()` does
  not **mint** a context, an id, or a suite on a session that negotiated
  nothing. **A future HTTPS-path fix will legitimately move 0 of these 89
  checks**, exactly as F18 §6.2 predicted for the 47.
* **`getPeerPrincipal().equals(peerCerts[0].getSubjectX500Principal())` is
  `true`** (F18 §2.1) — `getPeerPrincipal()` is an `X500Principal` and is the
  leaf certificate's subject, not a sibling accessor that happens to agree.
  Both doors here **refuse**, so that row has no vector without a handshake and
  is **not written**. See NOMINATION 1.
* **`getApplicationBufferSize()` measured 16704** on the null session (16676
  negotiated). **Deliberately not asserted**: F18 §8.3 records CratonVM's 16384
  as a knowing under-report, so a row would be red for a reason this fixture
  does not own. Recorded so the next reader does not "helpfully" add it.

An in-memory `SSLEngine`-to-`SSLEngine` handshake was considered as a way to
get a real session with no network, and rejected: the server side needs key
material that `SSLContext.init(null, null, null)` does not supply, and the
anonymous suites that would avoid it are disabled by default on JDK 25.

---

## 3. `RJdkSecurity` — F12-1 N1, the `SecureRandom` argument-kind vector, 80 → 123

### 3.1 Why a green `--only=random` was not evidence

F12-1 fixed **seven** `SecureRandom` sites and reported that **five of the
seven have no check anywhere in the tree**. `--only=random` drives
`java.util.Random` only. The `java.util.Random` half of the identical
null-argument defect had a check (`RJdkIntrinsics2` `random` check 41) and was
therefore caught; the `SecureRandom` half had none and was found only by
reading the file beside it.

The new `srArgKinds` family is organised **by argument kind, not by method**,
which is F12's own lesson and is what makes §3.3 reachable at all.

### 3.2 What is asserted, measured on jdk-25.0.3+9

| kind | call | answer |
|---|---|---|
| nullable ref | `nextBytes(null)` / `setSeed((byte[])null)` / `new SecureRandom((byte[])null)` | `NullPointerException`, **message null** on all three |
| nullable ref (control) | the same three with `new byte[0]` | **no throw** |
| ranged int | `generateSeed(-1)` and `generateSeed(MIN_VALUE)` | `IllegalArgumentException: numBytes cannot be negative` |
| ranged int | `generateSeed(0)` | `byte[0]` |
| ranged int | static `SecureRandom.getSeed(-1)` / `getSeed(0)` | same IAE + message / `byte[0]` |
| ranged int | `nextInt(0)` and `nextInt(MIN_VALUE)` | `IllegalArgumentException` |
| selector | `getInstance(null)` | `NullPointerException: null algorithm name` |
| selector | `getInstance("")` | `NoSuchAlgorithmException: " SecureRandom not available"` — **leading space** |
| selector | `getInstance("NO-SUCH-PRNG")` | `NoSuchAlgorithmException: NO-SUCH-PRNG SecureRandom not available` |
| selector | `getInstance("sha1prng")` | **resolves** — JCA lookup is case-insensitive |

The message is asserted only where the string is itself the finding:

* `generateSeed`'s wording was the **whole** of one defect — the class was
  already right and only `"numBytes must be non-negative"` vs
  `"numBytes cannot be negative"` separated them. A type-only row is blind to
  it. (Mutants A11/A15.)
* `"null algorithm name"` is the string the **pre-fix
  `IllegalArgumentException` also carried**, so there the class row is the
  discriminator and the message row pins that the correct message survived the
  class change. (Mutants A19/A20.)
* `" SecureRandom not available"`'s leading space proves the empty name
  travelled the ordinary template path rather than a special-cased early
  return, which is precisely what F12 §1(e′) deleted. (Mutant A22.)

### 3.3 The subtlest one — it only exists when TWO arguments are bad at once

All three two-argument `getInstance` overloads open with
`Objects.requireNonNull(algorithm, "null algorithm name")`
(`SecureRandom.java:439` for the `String` provider, `:481` for the `Provider`).
The provider is examined only afterwards.
`native_secure_random_get_instance_with_provider` ran `check_named_provider_arg`
**first**.

No per-method review reaches this: the method rejects *both* of its arguments
correctly, in the wrong order, so every single-bad-argument probe is green.

```text
getInstance(null, "SUN")            -> NPE  "null algorithm name"
getInstance(null, "NOPE")           -> NPE  "null algorithm name"     (provider-first: NoSuchProviderException)
getInstance(null, (String) null)    -> NPE  "null algorithm name"     (provider-first: IAE "missing provider")
getInstance(null, "")               -> NPE  "null algorithm name"     (provider-first: IAE "missing provider")
getInstance(null, (Provider) null)  -> NPE  "null algorithm name"     (third overload, separate registration)
getInstance(null, sunProvider)      -> NPE  "null algorithm name"
```

and the **controls**, without which every row above also passes a body that
answers NPE for anything it dislikes:

```text
getInstance("SHA1PRNG", (String) null)   -> IllegalArgumentException   (NOT NPE)
getInstance("SHA1PRNG", "")              -> IllegalArgumentException
getInstance("SHA1PRNG", "NOPE")          -> NoSuchProviderException
getInstance("SHA1PRNG", (Provider) null) -> IllegalArgumentException
getInstance("NOPE", "SUN")               -> NoSuchAlgorithmException   (the lookup IS after the provider)
getInstance("SHA1PRNG", "SUN")           -> no throw
```

Fixing the one-argument form does **not** fix this; the one-argument body is
reached only after both provider checks have had their chance to throw.

The provider-argument rows carry the **class only**. HotSpot's messages are
`"missing provider"`, `"no such provider: NOPE"` and
`"no such algorithm: NOPE for provider SUN"`, and CratonVM's were never
measured by F12 or by this lane, so asserting them would be writing an
expectation nobody has checked against the VM. Recorded here instead — see
NOMINATION 3.

### 3.4 The row that catches the provider being IGNORED, and the tautology it replaces

The obvious row — `getInstance("SHA1PRNG", sunProvider).getProvider()` is
`"SUN"` — **is a tautology on this pair**, because SUN is also the *default*
provider for SHA1PRNG. A body that drops the provider argument entirely
answers SUN too, so the row would pass against both the fix and its inverse.
That is the exact failure mode this directory keeps recording, and it was
caught here by trying to write the mutant for it.

The discriminator is a provider that is **installed and does not serve the
algorithm**. `SunJCE` ships in every OpenJDK and publishes **no** `SecureRandom`
service at all (measured: only `SUN` publishes `SHA1PRNG`/`DRBG`, and on this
Windows host `SunMSCAPI` publishes `Windows-PRNG` — which is why the row uses
SunJCE and not the MSCAPI one; SunJCE is portable and the MSCAPI one is not):

```text
getInstance("SHA1PRNG", "SunJCE")                        -> NoSuchAlgorithmException
getInstance("SHA1PRNG", Security.getProvider("SunJCE"))  -> NoSuchAlgorithmException
```

A body that resolves the algorithm and ignores the provider returns an object
here. Both overloads get the row, because they are separate registrations and
this tree has a recorded defect (`Cipher`/`SecretKeyFactory`) where exactly the
`Provider`-object form discarded its argument.

The `getProvider().getName() == "SUN"` row is **kept**, with its reason
rewritten: it does not catch the ignored provider (the two SunJCE rows do), it
catches the separately-recorded construction route that stamped `algorithm` and
never `provider`, so `getProvider()` came back **null**.

### 3.5 The family publishes its own denominator, and the tripwire was proved to fire

`RJdkSecurity` had no `sectionEnd` mechanism — one running counter and a class
total. The new family carries its own:

```java
int n = checks - mark;
if (n != 43) {
    throw new AssertionError("srArgKinds ran " + n + " checks, header says 43");
}
System.out.println("CK RJdkSecurity srArgKinds=" + n);
```

**Mutation-checked like any other assertion**: changing the literal to `44`
gives `AssertionError: srArgKinds ran 43 checks, header says 44` and no `PASS`
line. A denominator guard that cannot go red is worth nothing, and this
directory has shipped several.

### 3.6 Mutation results — 43 of 43 died

All 43 rows of `srArgKinds`, one mutant each, each mutant being what a named
wrong implementation answers. Selected ones, because they are the ones a
reviewer should check are really there:

| # | row | mutated to | |
|---|---|---|---|
| A1/A3/A5 | the three NPEs | `none` — **the swallowing body**, which is what shipped | DIED |
| A2/A4/A6 | their null messages | `java.util.Random`'s helpful-NPE text | DIED |
| A7–A9 | the `new byte[0]` controls | NPE — a body that rejects EVERY array | DIED |
| A11/A15 | `generateSeed`/`getSeed` message | `"numBytes must be non-negative"` — **the literal pre-fix string** | DIED |
| A12 | `generateSeed(MIN_VALUE)` | `none` — a guard written around `-1` rather than `<= 0` | DIED |
| A19 | `getInstance(null)` | `IllegalArgumentException` — **the pre-F12 answer, same message** | DIED |
| A21/A22 | `getInstance("")` | `IAE` / `"null algorithm name"` — the pre-fix early return | DIED |
| A24 | `getInstance("sha1prng")` | `NoSuchAlgorithmException` — a case-sensitive table | DIED |
| A27/A28 | `getInstance(null, "NOPE")` | `NoSuchProviderException` / `"no such provider: NOPE"` — **the provider-first body, class and message** | DIED |
| A29–A32 | the other null-algorithm rows | `IllegalArgumentException` — the provider-first body's other arm | DIED |
| A33/A34/A36 | the IAE controls | `NullPointerException` — **the over-broad "throw NPE on any null" fix** | DIED |
| A39/A40 | the two SunJCE rows | `none` — **the provider argument dropped** | DIED |
| A42 | `viaProvider.getAlgorithm()` | `"Unknown"` — the fabricating construction route | DIED |

---

## 4. `INDEX.md` brought current

The index was lane C18's snapshot: **155 files, 2026-08-13 00:07**, with its own
banner saying it would rot. It had. Listing re-taken: **227 `.md` files**
including the index, and **73 had no row**.

Added as a dated second-pass block rather than merged into C18's topic tables,
so C18's snapshot stays legible as the snapshot it is:

* **the SSL/TLS session chain, as a chain** — E12-1 → E22-1 → E31-1 → E42-1 →
  F6-1 → F10-1 → F18-1, in reading order, each row saying what that record
  established and what the next one moved. **Seven, not six.** A reader opening
  any single one of them gets a picture a later record has already corrected —
  E12-1's fix went into a registrar that does not answer (E22-1); E22-1's
  fixture ran 1 of 47 checks (E31-1); E31-1's slot did not exist at that width
  (E42-1); "invalidate moves isValid and nothing else" is wrong (F18-1).
  `E3-1` and `D3-3` are listed as adjacent, not as part of the chain.
* wave D, wave E (defect/census), the **E-`R11`** baseline-and-guards line, the
  **`W8-E`** harness/oracle line, wave F, and the two C19 fixture stragglers.
* a **once-stated provenance convention** for waves D/E/F — HotSpot MEASURED,
  tree and `jdk25src` READ, CratonVM PREDICTED — rather than the same sentence
  73 times, with the exceptions named (`E32`/`E37`/`E41-R11` ran `cargo test`;
  the `W8-E` records ran the suite).
* a restatement that `FIXED-UNVERIFIED` in this directory means *no binary
  carrying the change has ever executed* — several of these records also say
  they were never type-checked.

The new block carries the same rot warning, and it is not decorative: lanes
F14–F25 were landing while it was written.

---

## 5. What was NOT changed, and why

1. **`regression-suite/run.sh`** — not this lane's file. **No nomination is
   needed**: `RJdkIntrinsics2` and `RSslNullSession` are already in
   `CORE_CLASSES` and `RJdkSecurity` is already in `JDKONLY_CLASSES` (lines 164
   and 205). All three edits are in-place; no class was added or renamed, so
   nothing in `run.sh` refers to anything that moved. Re-checked at the end of
   the lane: `run.sh` was dirty from a sibling lane when this lane opened and is
   clean again now, with both registration lines intact — this is a **shared
   worktree** and that file moves under you.
2. **`regression-suite/src/RJdkReflBox.java`** — a live lane is creating it.
   Not read, not touched.
3. **Nothing under `native-*/src/`.** No Rust file was opened for edit. The
   three fixtures are the whole of the executable change.
4. **`RJdkSecurity` still prints no `failures=` line.** It throws
   `AssertionError` on the first divergence instead, so `failures` would be
   `0` on every line it ever prints. Pre-existing, one value per line, and
   `harness_check_count`'s `sub(/^.*checks=/, "")` reads its `checks=` line
   correctly. Left alone deliberately: changing a fixture's reporting shape is
   a separate change from adding rows to it.
5. **`RSslNullSession`'s NO-NETWORK property.** Preserved. §2.5.

---

## 6. Residuals — the honest list

1. **No CratonVM run of any kind.** All 86 mutation results and all three
   `PASS` lines are HotSpot. What these fixtures report on CratonVM is unknown
   to this lane.
2. **`bounds` aborts at its first failure.** On CratonVM, rows 103–121 do not
   execute until 1–102 pass. A green `bounds` is evidence; a red one says
   nothing about the nineteen.
3. **Rows most likely to be the first red on CratonVM**, named so the next lane
   does not have to guess:
   * `RSslNullSession`'s three `attrs.shadow.*` rows — they are the executable
     form of a **recorded, unfixed** defect (E31-1 §2's width-blind slot 3). If
     the fix did not land, these go red **by design**.
   * `RSslNullSession`'s `invalidate` arm at the **engine** door. F18's bit is
     keyed on the session object, but the two doors mint different widths and
     the width table is where this family keeps failing.
   * `RJdkSecurity`'s two SunJCE rows, if the provider argument is still
     discarded on either overload.
4. **`SecureRandom.getInstance("sha1prng").getAlgorithm()` is `"sha1prng"` on
   HotSpot** — the spelling **as asked**, not normalised. Measured, and
   **deliberately not asserted**: F12 §3.5 records that
   `secure_random_static_provider` normalises to upper-case alphanumeric for
   the *lookup*, and says nothing about what `getAlgorithm()` then reports. A
   row here would be an expectation nobody has checked against the VM. See
   NOMINATION 2.
5. **Seeded-SHA1PRNG determinism is not asserted.** Measured on HotSpot:
   `setSeed(new byte[]{1,2,3,4})` then `nextBytes(new byte[16])` gives
   `f24d7b797432a7aaf05c29e032faa297`, reproducible across instances. Out of
   scope for an *argument-kind* vector, and the fixture's header states a policy
   of asserting `SecureRandom` output only through invariants. Recorded so the
   value does not have to be re-measured.
6. **`SSLSession.putValue`/`getValue`'s own null contract is not asserted.**
   Measured on HotSpot: `putValue(null,"v")` and `putValue("k",null)` are
   `IllegalArgumentException: arguments can not be null` (**plural**), and
   `getValue(null)` is `IllegalArgumentException: argument can not be null`
   (**singular** — two different strings, one letter apart, from two methods).
   Not written: CratonVM's answers were never measured and these are not among
   F18's four doors. NOMINATION 4.
7. **`socket.getSession() != socket.getSession()`** on an unconnected
   `SSLSocket` (a fresh object each call), while
   `engine.getSession() == engine.getSession()`. Measured, surprising, and not
   asserted — this lane could not establish whether the socket-side answer is
   contract or accident, and F18 §8.3(4) records a *different* identity
   question (`HttpsURLConnection.getSSLSession()`) that is already open.

---

## 7. NOMINATIONS

### NOMINATION 1 — an HTTPS-path fixture for the half `RSslNullSession` cannot reach

Not this lane's to create, and not `RSslNullSession`'s job — that file's
no-network property is load-bearing. Two measured facts have **no vector
anywhere in the tree**:

* `invalidate()` drops `getSessionContext()` from `SSLSessionContextImpl` to
  `null` **on a session that negotiated** (F18-1 §3.1). Four comments across
  three files still say "isValid and nothing else".
* `getPeerPrincipal().equals(peerCerts[0].getSubjectX500Principal())` is
  `true`, and `getPeerPrincipal()` is a `javax.security.auth.x500.X500Principal`
  (F18-1 §2.1).

`scratchpad/f18/F18SessionContract.java` ARM C is the shape; it wants a home
under `regression-suite/` or `probes/` with a loopback `HttpsServer`.

### NOMINATION 2 — `RJdkSecurity`, the algorithm-spelling row (needs one CratonVM run first)

Exact literal text to add, **after** the case-insensitive lookup row in
`secureRandomArgumentKinds()`, **only once someone has run it on CratonVM** —
and if it is red, that is a finding for `securerandom.rs`, not a reason to drop
the row:

```java
        check("sha1prng".equals(SecureRandom.getInstance("sha1prng").getAlgorithm()),
                "getAlgorithm() reports the spelling that was ASKED FOR, not a"
                        + " normalised one — JCA lookup is case-insensitive and the name"
                        + " is carried through verbatim; measured \"sha1prng\" on"
                        + " jdk-25.0.3+9");
```

The denominator moves **43 → 44**; the literal in the `if (n != 43)` tripwire
and its message must move with it, or the family throws.

### NOMINATION 3 — `RJdkSecurity`, the three provider messages (same precondition)

The class-only rows in `getInstanceArgumentOrder()` can each gain a message
once CratonVM's strings are known. Measured on jdk-25.0.3+9:

```text
getInstance("SHA1PRNG", (String) null)    IllegalArgumentException: missing provider
getInstance("SHA1PRNG", "")               IllegalArgumentException: missing provider
getInstance("SHA1PRNG", (Provider) null)  IllegalArgumentException: missing provider
getInstance("SHA1PRNG", "NOPE")           NoSuchProviderException:  no such provider: NOPE
getInstance("NOPE", "SUN")                NoSuchAlgorithmException: no such algorithm: NOPE for provider SUN
getInstance("SHA1PRNG", "SunJCE")         NoSuchAlgorithmException: no such algorithm: SHA1PRNG for provider SunJCE
```

Each row added moves the `srArgKinds` denominator by one.

### NOMINATION 4 — `RSslNullSession`, the attribute null contract (same precondition)

Measured, §6.6. Four rows, and the pair worth having is the **singular/plural**
one: `putValue` says `"arguments can not be null"` and `getValue` says
`"argument can not be null"`. Two methods, two strings, one letter apart —
a single shared constant would be wrong in one of the two places, and only a
message row can see it.

### NOMINATION 5 — F21-1's own N2, still open

`probes/BufferAccessibleArrayProbe.java` / `.expected.txt` has four contagion
rows and they are CharBuffer-only. The seven-family sweep is
`scratchpad/f21/F21ViewContagionProbe.java`; promoting it with its 916-row
transcript gives the next lane F21-1 §1's tables without re-measuring. Not this
lane's files.

---

## How to verify

1. **Oracle, all three:**
   ```
   javac -d regression-suite/build regression-suite/src/RJdkIntrinsics2.java \
       regression-suite/src/RSslNullSession.java regression-suite/src/RJdkSecurity.java
   java -cp regression-suite/build RJdkIntrinsics2   # bounds=121, PASS (1022 checks)
   java -cp regression-suite/build RSslNullSession   # failures=0, PASS (89 checks)
   java -cp regression-suite/build RJdkSecurity      # srArgKinds=43, PASS (123 checks)
   ```
2. **CratonVM, which this lane could not run.** `--real-jdk` and `--jdk-only`
   for `RJdkSecurity`; `--real-jdk` for the other two. Read §6.3 first: three
   groups of rows are the expected first reds and each names the record that
   would explain it.
3. **The tripwires are real.** Change `sectionEnd("bounds", 121)` to `122`, or
   `if (n != 43)` to `44`, and confirm each throws. Both were checked this way.
4. **Re-take the index listing before quoting its count.** It said 155 and the
   directory held 227.
