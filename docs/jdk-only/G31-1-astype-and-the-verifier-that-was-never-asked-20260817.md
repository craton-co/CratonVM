# G31-1 — `asType` had no convertibility check, and the verifier that was never asked

**Status:** FIXED-UNRUN. The two fixes below are written and the tree they are
in has been compile-checked by the orchestrator (`cargo check --workspace
--tests` clean), but **no binary carrying them has ever executed**. This lane
was not permitted to build. Every "before" is MEASURED on an attributable
binary; every "after" is PREDICTED and must be re-measured before it is
believed — this directory's own history (HANDOFF §2, §5) is that a green build
proves you broke nothing, not that you did something.

**Provenance:** MEAS on both VMs. HotSpot 25.0.3+9-LTS (Temurin, this host) is
the oracle throughout. CratonVM "before" measured on
`C:/craton/target-rel2/release/cratonvm.exe` (from `9964ca733`) and
independently reproduced on `target-fcheck`; the two agree row for row.
Probes: `scratchpad/g31/{AsTypeFamily,AsTypeExtra,Sup,HvFamily,HvCase,HvLambda,HvReadback}.java`.
JDK sources read from `$JAVA_HOME/lib/src.zip`.

Files: `native-builtins/src/lang_invoke.rs`,
`native-builtins/src/http_url_connection.rs`.

---

> **VERIFIED AGAINST A BINARY 2026-09-04. All seven named vectors are green, and
> three of this record's own check counts match exactly.** Status was
> **FIXED-UNRUN**: *"no binary carrying them has ever executed"*, with §5 noting
> the vectors *"were re-run only in their BEFORE state."*
>
> ```text
>                            HotSpot 25                    CratonVM --jdk-only         differing CK
> RJdkProxyIface             PASS (38 checks, 9 steps)     PASS (38 checks, 9 steps)        0
> RSslLiveSession            PASS (95 checks)              PASS (95 checks)                 0
> RJdkHandles                PASS (331 checks, 40 steps)   PASS (331 checks, 40 steps)      0
> RJdkLambdas                PASS (38 checks)              PASS (38 checks)                 0
> RJdkFunctionCombinators    PASS (452 checks)             PASS (452 checks)                0
> RSslNullSession            PASS (89 checks)              PASS (89 checks)                 0
> RJdkNet                    PASS (81 checks)              PASS (81 checks)                 0
> ```
>
> **The counts are the corroboration.** This record names three of them —
> `RJdkHandles` **331**, `RSslNullSession` **89**, `RJdkNet` **81** — and all
> three match. These are therefore the vectors it measured, not vectors that
> drifted underneath the record, which is the usual reason a count in a
> three-week-old page cannot be compared with anything.
>
> **The three it flagged as at risk are byte-identical to the oracle.** §5 says
> *"`RJdkHandles`, `RJdkLambdas` and `RJdkFunctionCombinators` are the ones at
> risk from fix 1 — they are the vectors that exercise `asType` hardest — and
> §1.5's under-refusal exists because of them, not in spite of them."* The
> under-refusal cost them nothing measurable here.
>
> **What this does NOT verify — §5's other two items are untouched.** Whether
> `class_name_of_id` should resolve a hidden class at all is still unswept: the
> same `None` presumably still reaches every other native that asks, and no
> probe here looks. NOMINATION 1, `getSSLSession()` identity, is *"diagnosed and
> located but not fixed"*, and `RSslLiveSession` passing is NOT evidence against
> it — §5 calls that defect *"the next thing `RSslLiveSession` will report"*,
> i.e. one this vector does not currently reach. §2's `HttpsURLConnection`
> verifier analysis is a source and oracle argument and was not re-derived.

## 0. The headline

| vector / row | before (MEASURED) | after (PREDICTED) |
|---|---|---|
| `RJdkProxyIface` | 37 of 38 checks, `FAIL step refusals: AssertionError: unreachable` | 38 of 38 |
| `RSslLiveSession` | 67 rows, `FAILED phase=verifier: SSLPeerUnverifiedException` | past `verifier.hostArg`; next failure expected at `verifier.sameObjectAsGetSSLSession` (§6, NOMINATION 1) |
| `asType` conversion rule | no check of any kind existed | 917 of 917 measured oracle cells |
| per-connection lambda `HostnameVerifier` | never consulted | consulted |

Two independent defects, one theme: **a predicate that folded "I do not know"
into "the answer is no".** `asType` did not know how to check convertibility so
it checked nothing; `is_default_hostname_verifier` could not name a lambda's
class so it called it a JDK default. Both refuse to distinguish absence from
ignorance, and both were invisible until something asked.

---

## 1. `MethodHandle.asType` — the passthrough that converted anything to anything

`native-builtins/src/lang_invoke.rs`. The registered body wrote the requested
`MethodType` into the receiver's `type` field and returned the receiver. There
was no convertibility check at all, so a `(String)String` handle became a
`(int,int)int` one on request.

It IS the live body, and this was worth re-establishing: an earlier record
concluded it was not, from a dump showing `invocations=0` — taken on a workload
that never calls `asType`. Dumped against `RJdkProxyIface` itself:

```text
java/lang/invoke/MethodHandle.asType
  registered_by=native-builtins/src/lang_invoke.rs:11491
  owns_slot=true   invocations=24   overwrote=None
java/lang/invoke/MethodHandles.explicitCastArguments
  registered_by=native-builtins/src/lang_invoke.rs:7404
  owns_slot=true   invocations=0    overwrote=None
```

`invocations` is workload-dependent and its magnitude is separately unreliable;
`owns_slot` is the trustworthy field. `MethodHandleProxies` is NOT registered
natively, so the real JDK bytecode runs and reaches this native twice — at
`asInterfaceInstance` line 188 and again from the generated proxy's
`callerBoundTarget.asType(xxType)` at line 381 — which is why the missing check
surfaced as a missing refusal rather than a wrong call.

### 1.1 The rule, and the 917 cells it was checked against

Transcribed from `MethodType.canConvert` (src.zip lines 1078-1128) and
`MethodType.isConvertibleTo` (986), then checked cell by cell against a sweep of
the oracle: a 324-cell return matrix and a 289-cell parameter matrix
(`AsTypeFamily`), plus 304 wrapper-supertype cells (`AsTypeExtra`). The
implementation was extracted and run against all 917 rows standalone:
**`checked=613 mismatches=0`, `extra checked=304 mismatches=0`.**

Three things that sweep settled which are not guessable from the doc:

1. **Reference → reference is ALWAYS convertible.** `String → Integer`,
   `int[] → String`, `Void → Comparable` — every cell. Null is always
   dynamically valid, so the cast is deferred to invoke time.
2. **`void` converts in BOTH directions** as a return type. The whole `void`
   row and the whole `void` column are accepts.
3. **The reference → primitive arm has THREE tests, and the third is the one
   that gets left out.** `Byte → short` and `Character → int` are accepts (unbox
   from a strongly typed wrapper, then widen) while `Number → char` is a
   refusal. Dropping that arm turns 20 measured accepts into refusals.

The wrapper supertype table is CLOSED, which is what makes the check possible
without a hierarchy walk (`NativeContext::is_subclass` cannot answer for
interfaces). Enumerated by reflection (`Sup.java`) and cross-checked against the
matrix:

```text
  Boolean, Character  <: Object Comparable Serializable Constable
  Byte, Short         <: Object Comparable Serializable Constable Number
  Integer, Long,
  Float, Double       <: Object Comparable Serializable Constable Number ConstantDesc
  Void                <: Object                                              (only)
```

`ConstantDesc` covering four wrappers and not six is the row inspection gets
wrong: `Byte` and `Short` are `Constable` but not `ConstantDesc`, and the
measured matrix agrees (`ConstantDesc → int/long/float/double` accept,
`→ byte/short/char/boolean` refuse).

**The direction is reversed for parameters and that is not a typo.** The return
travels old → new; each parameter travels new → old. A reversed implementation
passes every widening row and fails every narrowing one, which reads like an
off-by-one rather than a reversal.

### 1.2 The message is transcribed, and so is its rendering

`MethodHandle.asTypeUncached`: `"cannot convert " + this + " to " + newType`,
where `MethodHandle.toString()` is the literal `MethodHandle` followed
immediately by its `MethodType` — no space. Measured forms:

```text
  cannot convert MethodHandle(String)String to (int,int)int
  cannot convert MethodHandle(int)void to ()void
  cannot convert MethodHandle(long,long)long to (int,int)int
  cannot convert MethodHandle(Inner)int[] to (Inner)int
```

`MethodType.toString()` uses simple names, keeps array brackets, and prints a
nested class by its inner name alone: `(String[],Object[][])int[]`, `(Entry)Inner`,
`()void`. All four renderings are asserted character for character in the unit
tests.

### 1.3 The trap: `explicitCastArguments` has different rules

Both of its matrices are accepts in **every** cell — `String → boolean`,
`double → char`, `int[] → long`, all of them — because it inserts an explicit
cast rather than demanding a lossless conversion. Its only refusal is a
parameter COUNT mismatch, and its message says "explicitly cast", not "convert":

```text
  cannot explicitly cast MethodHandle(int)void to ()void
```

Reusing `asType`'s predicate there would refuse 248 pairs HotSpot performs. The
body checks arity and nothing else.

### 1.4 The other measured rows

| row | HotSpot |
|---|---|
| `asType` to the identical type | returns **`this`** |
| `asType` to an `equals`-identical type | returns **`this`** |
| `asType(null)` | `NullPointerException: Cannot invoke "java.lang.invoke.MethodType.form()" because "newType" is null` |
| `asInterfaceInstance(String.class, h)` | `IllegalArgumentException: not a public interface: java.lang.String` |
| `asInterfaceInstance(Subtractor.class, null)` | `NullPointerException` (null message) |
| `asInterfaceInstance` with a matching handle | proxy is created and invokes |
| `explicitCastArguments` to the identical type | returns `this` |

`asType(null)` was a silent passthrough here — a caller asking for an adaptation
it did not describe got an unadapted handle. Now refused with the transcribed
message.

### 1.5 What the fix deliberately does NOT do, and the under-refusal it buys

**HotSpot's `asType` returns a NEW handle and leaves the receiver alone.**
MEASURED: `identity(int).asType((int)long)` answers `(int)long` while the
receiver still answers `(int)int`, and `adapted == receiver` is **false**.
CratonVM has always had ONE object — the body mutates `type` in place — so a
second `asType` on the same reference sees the first one's adapted type where
HotSpot would still see the original.

Adding a check on top of that aliasing would manufacture refusals HotSpot never
issues. So a conversion the raw `MH_DESC` bytecode signature would have allowed
is accepted even when the mutated `type` field forbids it. That is a deliberate
under-refusal, confined to exactly the cases the aliasing creates, and it cannot
turn an accept into a refusal. The check also declines entirely when the
receiver's `type` is the `MH_DESC` fallback, or either descriptor fails to parse
or render: **every way of not knowing is an accept**, because a missing refusal
is the state the VM was already in while a spurious one breaks working
`invokedynamic` call sites.

Collapsing the aliasing is NOMINATION 2.

---

## 2. `HttpsURLConnection` — the per-connection verifier that was never asked

`native-builtins/src/http_url_connection.rs`. `RSslLiveSession` progressed from
`FAILED phase=startup` with 0 rows to `FAILED phase=verifier` with 67 rows once
a sibling lane fixed the socket layer: a full live TLS 1.3 handshake now
completes in both directions, and the new first failure was
`SSLPeerUnverifiedException` at `RSslLiveSession.verifier:710`.

### 2.1 It was not "the verifier is ignored" — it was ignored for LAMBDAS only

The probe uses a certificate with a `localhost` dNSName SAN and deliberately no
iPAddress SAN, so `https://127.0.0.1:<port>/` fails the built-in check and the
verifier is reached. Same connection shape, same request, two verifiers:

```text
                                    HotSpot            CratonVM (before)
  named class  HvFamily$Rec         calls=1            calls=1   verifier=Some("HvFamily$Rec")
  lambda       (h,s) -> true        calls=1            calls=0   verifier=None
```

A lambda's runtime class is a hidden class and `class_name_of_id` answers `None`
for it. `is_default_hostname_verifier` folded `None` into "a JDK/VM default
stand-in is installed", the connection was refused, and every verifier written
the way applications actually write them took that path —
`RSslLiveSession.verifier` installs a lambda.

**The alternative diagnosis is ruled out, not argued away.** `verifier=None` in
the existing debug line was ambiguous: it printed the resolved class name, so
"no verifier object" and "a verifier object with an unresolvable name" looked
identical. `HvLambda decide` separates them — install a NAMED verifier
process-wide via `setDefaultHostnameVerifier` AND a lambda on the connection. If
the instance-field read had returned nothing, `huc_hostname_verifier`'s static
fallback would have found the named one and called it:

```text
  CratonVM: lambda.calls = 0   namedDefault.calls = 0
```

Neither ran. The instance read returned the lambda; only the naming failed. The
debug line now prints `<none installed>` and `<installed, class name
unresolvable>` as different strings, because a trace that cannot separate a
missing thing from an unnameable one is how this stayed hidden behind a trace
that was already on.

### 2.2 The three exits are three different outcomes, not one

`huc_unverified_peer_message`'s own doc argued that the ways to reach a failed
endpoint identification "cannot describe the same outcome three different ways"
and gave them one message. MEASURED — one case per process, so a refusal cannot
poison the next row (`HvCase.java`):

| case | HotSpot | CratonVM (before) |
|---|---|---|
| verifier returns `true` | 200 | 200 |
| verifier returns `false` | `java.io.IOException: Wrong HTTPS hostname: should be <127.0.0.1>` | `SSLPeerUnverifiedException: Certificate for <127.0.0.1> does not match…` |
| verifier throws | `java.lang.RuntimeException: java.lang.IllegalStateException: verifier exploded` | `SSLPeerUnverifiedException: the installed HostnameVerifier threw…` |
| no verifier installed | `SSLHandshakeException: (certificate_unknown) No subject alternative names matching IP address 127.0.0.1 found` | `SSLPeerUnverifiedException: Certificate for <127.0.0.1> does not match…` |
| control: matching host, verifier installed | not called (`calls=0`) | not called (`calls=0`) |

Three exception classes, three sentences. The `false` row is fixed here: it now
routes through its own sentinel to a **plain `java.io.IOException`** carrying
`Wrong HTTPS hostname: should be <host>` — `HttpsClient.checkURLSpoofing`'s own
`formatMsg("Wrong HTTPS hostname%s", …)`, SOURCE-VERIFIED. That type is not
`SSLPeerUnverifiedException`: `SSLPeerUnverifiedException` is what
`checkURLSpoofing` *catches and swallows* on its way to consulting the verifier,
not what it throws afterwards.

The `throws` and `no verifier` rows are NOMINATIONS 3 and 4 — neither is fixable
in this file.

### 2.3 What HotSpot hands the verifier, and when

MEASURED, and CratonVM already matches every cell of this except the last:

```text
  verifier.hostArg     = 127.0.0.1            (the URL host, not the SNI name)
  session.isValid()    = true
  session.getPeerPrincipal()   = CN=localhost
  session.getPeerCertificates().length = 1
  verifier session == getSSLSession()  = true   <-- CratonVM: FALSE
  control (built-in check passes)      = verifier never called on either VM
```

The ordering is confirmed as a FALLBACK, not a gate, on both VMs and in the JDK
source: `HttpsClient.afterConnect` runs `HostnameChecker.match` first and
returns without calling the verifier when it passes. An application "pinning"
with a `HostnameVerifier` on `HttpsURLConnection` is not consulted at all while
the name matches — on HotSpot exactly as here.

### 2.4 The registrar collision, re-checked

`http_url_connection.rs:404` defines `register_https_session_accessors` with the
same name as one in `net_phase_e.rs`, and runs later (lib.rs 18805 after 18688),
so it owns five of the six HTTPS session accessors and net_phase_e's copies are
dead; `getSSLSession` is the one name it does not register, so net_phase_e's
survives for it alone. **Confirmed against a `--dump-native-registry` dump on
this binary, which is what the existing comment asked for and had never had.**
No registration was added or moved by this change, so the collision is untouched
— but it is the mechanism behind NOMINATION 1.

---

## 3. What was fixed

1. `lang_invoke.rs` — `asType` now refuses non-convertible adaptations with
   HotSpot's transcribed `WrongMethodTypeException`, and refuses `asType(null)`.
   The rule is nine pure functions of descriptor strings
   (`mh_can_convert`, `method_type_is_convertible_to`, `method_type_display`, …),
   unit-tested without a VM — the only form in which a lane that may not build
   can check anything at all.
2. `lang_invoke.rs` — `explicitCastArguments` refuses an arity mismatch, and
   ONLY an arity mismatch, with its own transcribed message.
3. `http_url_connection.rs` — an application `HostnameVerifier` whose class
   cannot be named is no longer misread as a JDK default stand-in.
4. `http_url_connection.rs` — a verifier that DECLINED now produces HotSpot's
   plain `IOException` and its own message, split from the "certificate never
   matched" exit.
5. `http_url_connection.rs` — the `dbg-tls-auth` trace distinguishes a missing
   verifier from an unnameable one.

---

## 4. NOMINATIONS

1. **`net_phase_e.rs` + `http_url_connection.rs` — one session object, not
   two.** `huc_verify_hostname` allocates a fresh `SSLSession` to hand the
   verifier while `getSSLSession()` is served from net_phase_e's
   `https_session_object`. MEASURED: HotSpot hands out the SAME object
   (`verifier session == getSSLSession()` is true); CratonVM answers false. This
   is `RSslLiveSession.verifier.sameObjectAsGetSSLSession` and is the **next
   assertion that vector will hit** once fix 3 lands. It cannot be fixed from
   this file alone: net_phase_e must expose the cached object, and G7-1 already
   nominated collapsing the two registrars and the two tables into one. Do that
   first.
2. **`lang_invoke.rs` (this file, deliberately deferred) — `asType` must mint a
   new handle.** MEASURED: HotSpot's `asType` leaves the receiver's `type()`
   untouched and returns a different object. Reproducing that means building a
   second `MethodHandle` carrying every synthetic dispatch slot of an arbitrary
   handle kind, and no lane that cannot build the VM should attempt it. Until
   then §1.5's under-refusal stands in for it.
3. **TLS layer (`t27_tls.rs` / rustls integration) — endpoint identification
   must be able to fail INSIDE the handshake.** With only the default verifier
   installed, HotSpot fails with `SSLHandshakeException: (certificate_unknown)
   No subject alternative names matching IP address 127.0.0.1 found`, before any
   application code sees a connected socket. CratonVM re-derives endpoint
   identification after the handshake, deliberately, because rustls skips it
   whenever Java `TrustManager`s are supplied. The message can be copied; the
   TIMING cannot.
4. **`perform`'s `Result<_, String>` cannot carry a pending Java exception.** A
   `HostnameVerifier` that THROWS is reported by HotSpot as
   `java.lang.RuntimeException` wrapping the original; CratonVM converts it to
   `SSLPeerUnverifiedException`. Fixing it needs the error channel to carry an
   `ObjectRef`, which is a structural change across the whole file.
5. **`t27_tls.rs` — `getDefaultHostnameVerifier()` before class init.** MEASURED:
   HotSpot answers `javax.net.ssl.HttpsURLConnection$DefaultHostnameVerifier`;
   CratonVM mints its own `javax.net.ssl.HostnameVerifier` stand-in because the
   static has not been populated yet.
6. **`t27_tls.rs` / VM constructor — `setDefaultHostnameVerifier` does not reach
   a later connection's instance field.** MEASURED (`HvCase none`): with a named
   verifier installed process-wide and no per-connection one, HotSpot calls it
   (`calls=1`) and CratonVM reads
   `HttpsURLConnection$DefaultHostnameVerifier` off the instance slot and
   refuses. Separately, `HvReadback` shows the instance field and the live static
   disagreeing about which one `getHostnameVerifier()` answers from:
   HotSpot's connection keeps the default captured at CONSTRUCTION
   (`oldConn.unchanged = false`), CratonVM follows the live static (`true`).
   Both rows point at the same place and neither is in the two files this lane
   owned.
7. **`http_url_connection.rs` (this file, not taken) — accessors before
   `connect()` silently connect.** MEASURED: HotSpot answers
   `IllegalStateException: connection not yet open` for `getPeerPrincipal`,
   `getServerCertificates`, `getCipherSuite` and `getSSLSession` on an
   unconnected connection; CratonVM performs the exchange
   (`https_ensure_exchanged`) and answers. Not taken because
   `RSslNullSession` (89) and `RJdkNet` (81) are currently green and this path
   is under both of them; it needs its own measured pass.
8. **`vm_exec.rs` — `check_override` does not list `isVarargsCollector`.** Noted
   in passing while reading the neighbouring registrations; pre-existing, not
   introduced here.

---

## 5. What this lane could NOT settle

- **Nothing here has run.** No binary carries these edits. The six vectors named
  for verification (`RJdkProxyIface`, `RSslLiveSession`, `RJdkHandles` 331,
  `RJdkLambdas`, `RJdkFunctionCombinators`, `RSslNullSession` 89, `RJdkNet` 81)
  were re-run only in their BEFORE state. `RJdkHandles`, `RJdkLambdas` and
  `RJdkFunctionCombinators` are the ones at risk from fix 1 — they are the
  vectors that exercise `asType` hardest — and §1.5's under-refusal exists
  because of them, not in spite of them.
- **Whether `class_name_of_id` should resolve a hidden class at all.** The fix
  here makes the verifier path correct regardless of the answer, but the same
  `None` is presumably returned to every other native that asks. That surface
  was not swept.
- **`getSSLSession()` identity.** NOMINATION 1 is diagnosed and located but not
  fixed; it is the next thing `RSslLiveSession` will report.
