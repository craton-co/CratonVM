# H13-2 — all four assigned JCA defects were already closed; the probe that proved it found two more

**Status: MIXED.** §1 is **MEASURED** (four recorded defects verified against
the current tree, one of them by a byte-exact 33-line HotSpot diff). §2 and §3
are **MEASURED** defects with **FIXED-UNVERIFIED** repairs: no binary carrying
`ca8f03069` or `b773038e2` has been built or run, and this lane is forbidden to
build. §4 is measured and **NOT fixed**.

**Date** 2026-08-20
**Lane** H13 (`--jdk-only` completion, wave H)
**Subject** `native-builtins/src/jca/**` — the four JCA defects the brief named,
and what asking about them properly turned up
**Worktree / base** as `H13-1`: cut at `26e4b5db4`, fast-forwarded to
`fe59bf9d9` (63 commits) before any edit
**Binary used for every measurement** `C:/craton/target-jdkonly-h2/release/cratonvm.exe`,
run `--jdk-only` with **no** shadow dial armed
**Oracle** JDK 25.0.3+9 at `C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot`,
resolved with `dirname $(dirname $(command -v javap))`

**Commits**

| SHA | Change | Direction |
|---|---|---|
| `ca8f03069` | `provider_chain.rs` — a `Delegate` shape for the `(Spi, String, Provider)` arity | **widens** (an `Ok(None)` becomes a delegate) |
| `b773038e2` | `message_digest.rs` — run `check_provider_ownership` | **narrows** (kept separate for exactly that reason) |

---

> **VERIFIED AGAINST A BINARY 2026-09-04.** §2 and §3 were **MEASURED** defects
> with **FIXED-UNVERIFIED** repairs — *"no binary carrying `ca8f03069` or
> `b773038e2` has been built or run, and this lane is forbidden to build."* Both
> commits are in this tree and a binary built from it now runs the probe this
> record is named for.
>
> `probes/ProviderLookupProbe.java` was not in the checkout; it was recovered
> from `6ddce7ecc` (under `fixed-suite-bugs/repros/` in the internal tree) and its blob
> confirmed present in this repo's object store, so it is committed content, not
> someone's uncommitted local edit.
>
> ```text
>                          lines   differing from HotSpot 25
> HotSpot 25 (oracle)        33     —
> CratonVM compatible        33     0
> CratonVM --jdk-only        33     0
> HotSpot self-diff, 2 runs         0   (the probe is deterministic)
> ```
>
> **§3's distinction — EXISTENCE versus OWNERSHIP — is the one to read**, since
> that is what `b773038e2` repaired, and it is byte-identical to the oracle on
> both arms:
>
> ```text
> MessageDigest.getInstance(SHA-256, ghost)     NoSuchProviderException: no such provider: …
> MessageDigest.getInstance(NoSuchAlgo, SUN)    NoSuchAlgorithmException: … for provider SUN
> MessageDigest.getInstance(SHA-256, SUN)       OK
> MessageDigest.getInstance(SHA-256, "")        IllegalArgumentException: missing provider
> ```
>
> A provider that exists but does not own the algorithm now answers
> `NoSuchAlgorithmException` naming the provider, and an unregistered name
> answers `NoSuchProviderException` — the two outcomes the record says were
> being conflated.
>
> **§2 IS NOT VERIFIED BY THE ABOVE — and on a second probe it is REFUTED.**
> `ProviderLookupProbe` registers no custom provider at all: every one of its
> rows goes through `SUN` or an unregistered ghost name, so a green there says
> nothing about §2.2's arity/wrapper mechanism. §2.3 states the falsifier
> plainly — *"Build, then run the §2.1 probe. If
> `MessageDigest.getInstance("H13MD","H13Prov")` still throws, the delegate did
> not construct."*
>
> `probes/CustomProviderSpiProbe.java` was written for exactly that and **the
> falsifier fires**, identically in Compatible and `--jdk-only`:
>
> ```text
>                                              HotSpot 25            CratonVM (both arms)
> Security.addProvider(L7P) position>0         true                  true
> provider.getService(MessageDigest,L7DIGEST)  non-null              non-null
> MessageDigest.getInstance(L7DIGEST)          OK provider=L7P       NoSuchAlgorithmException
> MessageDigest.getInstance(L7DIGEST, "L7P")   OK provider=L7P       NoSuchAlgorithmException
> MessageDigest.getInstance(L7DIGEST, provObj) OK provider=L7P       NoSuchAlgorithmException
> ```
>
> **The refusal keys on the SPI SHAPE, not on the registration shape**, and the
> probe carries the control that shows it. Three registrations, one VM:
>
> ```text
> L7DIGEST  legacy put() string   + bare MessageDigestSpi   REFUSED (3 overloads)
> L7SVC     putService(Service)   + bare MessageDigestSpi   REFUSED (2 overloads)
> L7SUB     legacy put() string   + extends MessageDigest   MATCHES HotSpot
> ```
>
> `L7SUB` is the BouncyCastle route §2.2 says was always fine — it satisfies
> `is_subclass` and never needs a `Delegate`. It passes, so the provider chain
> does enumerate a runtime-added provider and the probe is not simply broken.
> Both bare-SPI registrations fail, so the two registration APIs are not the
> variable. What is left is the `Delegate` wrapping path — §2.2's mechanism.
>
> **Narrowed, not localised, and the difference is stated on purpose.**
> `ca8f03069` and `b773038e2` are both ancestors of `HEAD`;
> `engine_delegate_shape_with_provider` is present at
> `native-builtins/src/jca/provider_chain.rs:1624` **with** its
> `java/security/MessageDigest` row and the 3-arg descriptor; the call site
> reaches it; and `java.security.MessageDigest$Delegate` really is in the image
> (`javap -p` on `java.base.jmod` confirms the `(MessageDigestSpi, String,
> Provider)` shape). So the fix is in the tree and something ahead of or inside
> its arm still returns `Ok(None)` — `third_party_service_class`,
> `build_jca_impl`, or the `new_object_initialized` itself. **Which one was NOT
> determined here**, and no warning is logged on the path, so the next reader
> gets a silent `Ok(None)` and should instrument those three before assuming any
> of them.
>
> **What else this note does not cover.** §4's three residuals are stated as
> measured-and-not-fixed and are untouched; §1's four assigned defects were
> already MEASURED and are not re-derived. `md.getProvider() == myProvider` —
> §2.3's own second residual — could not be reached at all, because the
> `getInstance` before it throws.

> **Probe caution, recorded because it produced two wrong readings first.** The
> first extended run reported the discriminator rows as agreeing with HotSpot.
> They had not run: `javac` had failed on a protected `putService`, the previous
> `.class` files were still on disk, and the runner scored them. The second run
> then threw out of an unguarded `getInstance` before the discriminator, leaving
> those rows UNTESTED while a line-diff rendered them as ordinary missing lines.
> The probe now catches that throw, and the runner refuses to run at all if
> `javac` fails.

## 0. What moves, in one table

| Change | Strict (`--jdk-only`) | Compatible | Shadow census |
|---|---|---|---|
| `ca8f03069` | **neither** | **neither** | **unchanged** |
| `b773038e2` | **neither** | **neither** | **unchanged** |

Neither commit adds, deletes or re-tags a registration, so per
`HANDOFF-20260820.md` §1 neither can move a mode: only retiring a `Bridge`-tagged
shadow does that. Both change what an already-registered native *answers*. The
shadow census printed on every strict arm should be byte-identical; **if it is
not, something in this record is wrong.**

## 1. The four assigned defects: all closed. Verified, not read.

The brief warned that re-deriving a fixed record is this directory's recorded
failure mode, so each was checked against the tree rather than against its own
page.

| # | defect | record | state |
|---|---|---|---|
| 1 | `getInstance` ignores the requested provider | `fixed-suite-bugs/springboot/core-spring-boot-keystore-provider-name-swallowed-20260723-FIXED.md` | **FIXED, re-measured today** |
| 2 | a third-party SPI registration recorded but never instantiated | `docs/known-issues/netty/openssl-key-material-and-engine-residuals-20260813.md` §A′ | **closed 2026-08-17**; residual in §4 |
| 3 | a base-class native shadowing overloads a provider subclass does not override | `fixed-suite-bugs/bug-bcjava-jca-provider-alias-lookup-and-attribution-20260816.md` | **FIXED 2026-08-16**; residual in §4 |
| 4 | EC server identity rejected on JDK PKCS#8 v1 keys | `fixed-suite-bugs/netty/ec-server-identity-rejected-jdk-pkcs8-v1-20260812-FIXED.md` | **FIXED 2026-08-12**, in `t27_tls.rs` — **not a path this lane owns** |

**#1 is measured, not read.** The repo carries the probe the fix was closed with
(`fixed-suite-bugs/repros/jca-provider-lookup-parity/ProviderLookupProbe.java`,
33 `getInstance(algorithm, provider)` shapes). Run today, `--jdk-only`, against
the oracle:

```text
lines cv=33 hs=33
=== DIFF (< HotSpot, > CratonVM) ===
=== diff rc=0 ===
```

Byte-for-byte. Every `NoSuchProviderException` / `NoSuchAlgorithmException` /
`IllegalArgumentException` shape, in both spellings.

**So the assigned half of this lane's task was already done, and the honest
report is that it was.** What follows is what the *verification* found, which is
the part that was not.

## 2. `ca8f03069` — a provider written to the documented JCA contract could not reach `MessageDigest`

### 2.1 The measurement

A probe provider of the ordinary shape — the shape every JCA tutorial and every
`Provider` javadoc example uses:

```java
public final class MyMd extends MessageDigestSpi { … }          // a BARE SPI
public final class MyProv extends Provider {
    public MyProv() { super("H13Prov", "1.0", "…");
        put("MessageDigest.H13MD", MyMd.class.getName()); … } }
Security.addProvider(new MyProv());
```

| call | HotSpot 25.0.3+9 | CratonVM |
|---|---|---|
| `Security.addProvider` / `getProvider("H13Prov") == p` | true | **true** |
| `p.getService("MessageDigest","H13MD")` | a `Service` | **a `Service`** |
| `MessageDigest.getInstance("H13MD","H13Prov")` | `MessageDigest$Delegate` | **`NoSuchAlgorithmException: no such algorithm: H13MD for provider H13Prov`** |
| `MessageDigest.getInstance("H13MD", p)` | `MessageDigest$Delegate` | **same exception** |
| `MessageDigest.getInstance("H13MD")` | `MessageDigest$Delegate` | **`NoSuchAlgorithmException: H13MD MessageDigest not available`** |

The registration is recorded, the service is found, and the engine still refuses.

### 2.2 The mechanism — an arity, in a table with one row

`build_third_party_engine` already handles a bare SPI: it wraps one in the JDK's
own package-private `<Engine>$Delegate`. The shape it wraps with comes from

```rust
fn engine_delegate_shape(engine_class: &str) -> Option<(&'static str, &'static str)> {
    match engine_class {
        "java/security/KeyPairGenerator" => Some((… , "(Ljava/security/KeyPairGeneratorSpi;Ljava/lang/String;)V")),
        _ => None,                                   // <- every other engine
    }
}
```

and `java.security.MessageDigest$Delegate`'s **only** constructor is
`(MessageDigestSpi, String, Provider)` — verified with `javap -p -s` against
JDK 25.0.3+9, not assumed. A different **arity**, so it cannot be added as a
row; it fell off `_ => None`, `build_third_party_engine` returned `Ok(None)`, and
the caller's own refusal stood.

This is the `[_ => default]` shape: **a defaulting helper with no reporting
caller.** The `_` arm is indistinguishable from "this provider registers
nothing", so the engine reports the provider's algorithm as absent.

**Why a bc-java-driven fix round did not find it.** BouncyCastle registers
classes that extend the **engine** (`BCMessageDigest extends MessageDigest`), so
they satisfy the `is_subclass` test and never reach the `Delegate` branch at all.
The one engine that *did* need the branch — `KeyPairGenerator`, for BC's
composite signatures — has the 2-arg ctor. The corpus that drove the work had no
standard-shaped provider in it, so the branch looked complete.
`[false everywhere = absence]` and `[reach ≠ defect]`, together.

### 2.3 The fix, and its shape

`engine_delegate_shape_with_provider`, tried **only where the two-argument table
declines**. The `Provider` object is built exactly as `build_real_spi_wrapper`
builds it (`find` → `make_provider`), since `getProvider()` is `final` on the
engine and the wrapper's own field is the only thing that can answer it.

**It can only widen.** Every engine that resolves today either passes
`is_subclass` (untouched) or is named by the 2-arg table (untouched). The new
arm is reached only where the function returns `Ok(None)` today, and `Ok(None)`
is exactly what produces the `NoSuchAlgorithmException` in §2.1.

**Two residuals, stated rather than hidden**, both in the commit message and the
code:

* it constructs `Delegate`, never `CloneableDelegate`, so an SPI that also
  implements `Cloneable` yields a digest whose `clone()` throws where HotSpot
  clones. `Delegate.of` picks between them; picking `Delegate` is the
  always-correct half.
* the `Provider` is a **made** object, so `md.getProvider() == myProvider` stays
  `false` where HotSpot says `true` — §4.

**Falsifier.** Build, then run the §2.1 probe. If
`MessageDigest.getInstance("H13MD","H13Prov")` still throws, the delegate did
not construct and the arm should return `Ok(None)` rather than a wrong type —
which is what it already does.

## 3. `b773038e2` — `MessageDigest` checked provider EXISTENCE and never OWNERSHIP

Same probe, the other direction:

```text
MessageDigest.getInstance("SHA-256", "H13Prov")     // H13Prov registers no digest
  HotSpot  -> NoSuchAlgorithmException: no such algorithm: SHA-256 for provider H13Prov
  CratonVM -> a working SHA-256
```

`md_get_instance_with_provider` ran `check_named_provider_arg` — which admits any
name `find()` knows — and went straight to its own table.
`KeyFactory` (`key_factory.rs:2955`), `Signature` (`signature.rs:1245`),
`Cipher` (`cipher.rs:4564`), `SecureRandom` and `Mac` all run
`check_provider_ownership`. **`MessageDigest` was the last engine that did not.**
This is the wrong-ACCEPT half of `E25-R11` §1.5's species, closed there on `Mac`
and left open here, and it is `[pin NEG half]`: §2 and §3 are the two halves of
one gate and fixing either alone leaves the other reading as correct.

**The narrowing's safety precondition is MEASURED, not assumed.** The gate reads
`get_service_entry`, the same table `Provider.getService` answers from, and the
same one `third_party_service_class` uses (so §2's H13MD still passes it). On
this binary:

```text
CK SUN/MessageDigest.SHA-256=SERVICE     CK SUN/MessageDigest.SHA3-256=SERVICE
CK SUN/MessageDigest.SHA-1=SERVICE       CK SUN/MessageDigest.SHA-384=SERVICE
CK SUN/MessageDigest.SHA-512=SERVICE     CK SUN/MessageDigest.NoSuchAlgo=null
CK SUN/MessageDigest.MD5=SERVICE         CK sunMessageDigestServices=15
```

— 13 of 13 lines identical to HotSpot. And `check_provider_ownership` is already
live on five sibling engines for these same providers, with
`ProviderLookupProbe` green (§1), so the gate demonstrably passes JDK providers.

**Residual risk, and why the commit is separate.** This is the only change in
the lane that *removes* an answer. A digest spelling that CratonVM's own
`algorithm_supported` accepts but that the service table does not carry for the
named provider would newly refuse. I could not run the 104-vector arm to rule
that out. **The commit is standalone so the building lane can `git revert
b773038e2` without touching `ca8f03069`.**

**Falsifier.** `SUITE=all` must stay at 99/104 and `--jdk-only` at 104/104. If a
digest vector reddens with `no such algorithm: X for provider Y`, this commit is
the cause and the seam is `get_service_entry` missing an alias.

## 4. Measured, NOT fixed — three residuals outside this lane's paths

Same probe, driving the SPI's own methods and counting them:

| assertion | HotSpot | CratonVM | where the fix lives |
|---|---|---|---|
| `Signature.getInstance("H13SIG","H13Prov")` returns | `Signature$Delegate` | `java.security.Signature` | `phases_early.rs` |
| `sig.getProvider() == myProvider` | **true** | **false** | `provider_chain::make_provider`'s callers |
| `MySig` constructed / init / update / sign all reached | yes | **yes** | — (works) |
| `KeyPairGenerator.getInstance("H13KPG","H13Prov")` returns | `KeyPairGenerator$Delegate` | `java.security.KeyPairGenerator` | `phases_early.rs` |
| `MyKpg` constructed | yes | **yes** | — |
| **`g.initialize(256)` reaches `MyKpg.initialize(int,SecureRandom)`** | **true** | **false** | `phases_early.rs` |
| `g.generateKeyPair()` | returns | **`NoSuchAlgorithmException: H13KPG KeyPairGenerator not available`** | `phases_early.rs` |

Read that KeyPairGenerator row carefully. **The SPI is constructed and then
thrown away**: `build_jca_impl` builds `MyKpg`, the delegate is not built, the
caller gets CratonVM's own `KeyPairGenerator`, and `initialize(int)` — the
convenience overload the subclass does not override — is answered by the native.
That is exactly the trap
`bug-bcjava-jca-provider-alias-lookup-and-attribution-20260816.md` recorded and
fixed **for BouncyCastle's shape**: `kpg_receiver_is_ours` discriminates on the
exact class name `java/security/KeyPairGenerator`, and the object handed back
here **is** exactly that class, so the guard says "ours" and the native wins.
`[1 site != retired]`: the guard is right and the object it is asked about is
wrong.

Three of these four rows live in `phases_early.rs`, which this lane does not
own. They are recorded here with their measurements so the owning lane does not
have to re-derive them.

## 5. What I did NOT verify

* **Nothing in §2 or §3 was compiled.** `cargo check`/`build`/`test` are
  forbidden to this lane. Both edits are argued from the code around them and
  from four existing call sites with the same shape; neither has been seen to
  compile.
* **No suite was run after either edit.** The 104/104, 99/104 and 63/64
  baselines in the brief are unmeasured by me.
* **`ca8f03069`'s `Cloneable` residual** — I did not write the
  `CloneableDelegate` half, and I did not measure how many real providers'
  digest SPIs implement `Cloneable`.
* **Defect #4 (EC PKCS#8 v1)** — verified only by reading its record and
  confirming the fix site (`native-builtins/src/t27_tls.rs`,
  `ec_pkcs8_splice_public_key`) exists. **Not run.** It is not in this lane's
  owned paths and I did not touch it.
* **`Signature`/`KeyPairGenerator` delegate shapes** — I deliberately did not
  add them. Both engines *work* today for a standard-shaped provider (the SPI is
  driven); changing their route would alter a green path, and this lane cannot
  measure the result. §4 records them for whoever can.
* **Whether any in-tree test asserts the refusals §2 and §3 change.** I grepped
  for the messages and found none, but a test asserting a *count* rather than a
  message would not surface that way.

## 6. NOMINATIONS

* **N1 — chain the cause in `kf_generate_private`
  (`native-builtins/src/jca/key_factory.rs:~3516`).** It drives the real
  `sun.security.rsa` SPI and, on failure, discards the throwable and mints
  `InvalidKeySpecException` with no cause and no JDK frames. HotSpot's own
  `RSAKeyFactory` chains. The existing doc comment defends the *outcome*
  ("no worse than the synthetic fail-closed path") and is silent about the
  diagnosis; `H13-1` §2.3 is a whole section that exists because of it. Same
  shape at the public twin, `:~3323`.
* **N2 — the `SpiOverloadProbe` of §2/§4 belongs in the arm.** ~20 `CK` lines,
  no JDK internals, no `--add-opens`: a `Provider` with three bare SPIs and a
  count of which SPI methods were actually reached. It discriminates
  "registered" from "instantiated" from "driven", which is three defects the
  corpus currently cannot tell apart. Nothing in `regression-suite/src/` registers
  a standard-shaped third-party provider today.
* **N3 — audit `engine_delegate_shape` against `javap` for every engine, not
  just the two now named.** `Mac`, `KeyAgreement`, `SecretKeyFactory`,
  `AlgorithmParameters`, `CertificateFactory` and `SecureRandom` each have a
  Delegate or an SPI-taking constructor, and each is one `_ => None` away from
  §2's failure. The audit is one `javap -p -s` per class.
* **N4 — `getProvider()` must return the caller's instance.** `make_provider`
  fabricates one, so `x.getProvider() == myProvider` is false across every
  engine. `E25-R11` §1.6 recorded it for `Mac` in 2026-08 and scoped the fix to
  a file that lane did not own; §4 measures it on `Signature` and
  `KeyPairGenerator` too. It is one defect with at least three faces and it has
  now been deferred twice for the same reason.
* **N5 — re-read the four "FIXED" records in §1 for their scope sentence.**
  Every one of them is genuinely fixed *for the shape its suite exercised*. Two
  of the four had a residual of exactly this kind still live. A "FIXED" line is
  a statement about a corpus, not about the tree.
