# H13-1 — the three JCA vectors are a THIRD mechanism: a map that answers `get` and reports empty

**Status: OPEN — MEASURED. No source change in this record.** Every number below
is a run of `C:/craton/target-jdkonly-h2/release/cratonvm.exe` (mtime
2026-08-20 17:32, stated by the lane brief to be `fe59bf9d9`) against the
HotSpot oracle at `C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot` — **JDK
25.0.3+9, resolved with `dirname $(dirname $(command -v javap))`, not copied out
of a record**. The source fixes this lane landed are in `H13-2`; nothing here
depends on them and nothing here was measured with them.

**Date** 2026-08-20
**Lane** H13 (`--jdk-only` completion, wave H)
**Subject** why arming `CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/concurrent/ConcurrentHashMap`
breaks `RCrypto`, `RJdkSecurity` and `RJdkX509Intercept` — three of the eight
non-collection vectors `H0-3` §3 found and nobody had read
**Worktree** `C:/craton/cratonvm/.claude/worktrees/agent-a3da509dd75d189bc`
**Base** cut at `26e4b5db4`, fast-forwarded to
`claude/jdk-only-mode-handoff-09b48c` = `fe59bf9d9` before any edit. The gap was
**63 commits**: the whole H wave (`H0-1`…`H0-5`, `H1`…`H8`), the `native-io`
fixes, the JIT stack-walk/direct-bind work, and `HANDOFF-20260820.md`. `H0-3`,
`H0-4` and `H0-5` — the three records this lane is a continuation of — are all
inside it.

---

## 0. The finding, first

`H0-5` named two mechanisms under the `HashMap` blast radius: **A**, a view
carrier minted with its outer reference left null; **B**, a
`cratonvm.synthetic.AnonymousObject$N` reaching a real array store or cast. It
asked whether anything else was down there.

**There is, and it is the one under the JCA.** Under the armed dial a real
`ConcurrentHashMap` does not go empty and does not go disconnected. It goes
**internally inconsistent**: the same instance answers `get`, `containsKey`,
`size` and `isEmpty` from state that does not agree, and *which* answer you get
depends on which door you came through and on what bytecode ran between the
writes. Every consumer sees a plausible, non-empty, wrong registry — so the
JDK's own consistency guards never fire and the failure surfaces hundreds of
frames away, wearing a JCA exception.

Three consequences, all measured, all in §2:

* `javax.crypto.JceSecurity.<clinit>` throws, which kills **every `Cipher`,
  `KeyGenerator`, `Mac` and `SecretKeyFactory` in the process**, permanently.
* `sun.security.util.KnownOIDs.name2enum` holds **1 entry where HotSpot holds
  590**, so every OID↔name resolution in the security stack misses.
* an RSA PKCS#8 private-key import fails, and the native reporting it
  **discards the real SPI's exception**, so the log says nothing about any of
  the above.

---

## 1. Setup, and why bare runs are legitimate for these three

```bash
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 \
CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/concurrent/ConcurrentHashMap \
  cratonvm.exe --java-home "$JDK" --jdk-only -cp <build> <Vector>
```

`H0-5` §2 is right that **a bare vector invocation is not the arm**, and it
names `RJdkModule` and `RJdkLogging` as the two that cannot be diagnosed this
way. It does not apply here, and that is checked rather than assumed:
`regression-suite/harness-guard.sh`'s `class_args`, `class_cp_extra` and
`class_cv_args` have **no arm** for `RCrypto`, `RJdkSecurity` or
`RJdkX509Intercept`, so `run.sh` hands them exactly the command line above.

**Controls, MEASURED, same binary, same session, dial unset:**

| vector | unarmed | armed |
|---|---|---|
| `RCrypto` | **PASS (57 checks)** | `ExceptionInInitializerError` |
| `RJdkSecurity` | **PASS (153 checks)** | `InvalidKeySpecException` |
| `RJdkX509Intercept` | **PASS (26 checks)** | `AssertionError` |

**The prefix is not half-armed, and that is measured too.** With
`--jdk-only-report`, the census lists **18** `ConcurrentHashMap` /
`$KeySetView` / `$EntrySetView` / `$ValuesView` rows — `<init>()V`, `<init>(I)V`,
`<init>(IFI)V`, `put`, `putIfAbsent`, `get`, `containsKey`, `isEmpty`, `size`,
`mappingCount`, `addCount(JI)V`, `keySet`, `entrySet`, `values`, and the four
view `size`/`iterator` rows — and **every one of them reads `bytecode-won`.
Not one reads `native-won`.** So the divergences below are not "some methods
were still native"; they are what the real JDK bytecode does on this VM.

## 2. The three vectors

### 2.1 `RCrypto` — `isEmpty()` is `true` for a map whose `size()` is `1`

```text
Exception in thread "main" java/lang/ExceptionInInitializerError
  at RCrypto.main(RCrypto.java:545)
  at javax/crypto/KeyGenerator.getInstance(KeyGenerator.java:288)
Caused by: java/lang/SecurityException: Can not initialize cryptographic mechanism
  at javax/crypto/JceSecurity.<clinit>(JceSecurity.java:116)
Caused by: java/lang/SecurityException: Missing mandatory jurisdiction policy files: unlimited
  at javax/crypto/JceSecurity.setupJurisdictionPolicies(JceSecurity.java:388)
```

`setupJurisdictionPolicies`'s last gate is

```java
if ((defaultPolicy == null) || defaultPolicy.isEmpty()) {
    throw new SecurityException("Missing mandatory jurisdiction policy files: " + …);
}
```

and `javax.crypto.CryptoPermissions.isEmpty()` is one line: `return
perms.isEmpty();`, where `perms` is
`private transient ConcurrentHashMap<String,PermissionCollection>`.

**Replayed outside the failing `<clinit>`** — the same two policy files, the
same `load()` per file, the same `getMinimum` fold, through
`--add-opens=java.base/javax.crypto=ALL-UNNAMED`:

| | HotSpot 25.0.3+9 | CratonVM, armed |
|---|---|---|
| `default_local.policy` → `perms.size()` | 1 | 1 |
| `default_US_export.policy` → `perms.size()` | 1 | 1 |
| folded `defaultPolicy` → `Map.size()` | 1 | **1** |
| folded `defaultPolicy` → `keySet()` | `[CryptoAllPermission]` | **`[CryptoAllPermission]`** |
| **`CryptoPermissions.isEmpty()`** | **false** | **true** |

The map is populated. It says so. `isEmpty()` says otherwise, and `isEmpty()`
is what the JDK asks. `JceSecurity` then latches an `ExceptionInInitializerError`
for the life of the process, so **`RCrypto` does not fail at one check — the
entire JCE is dead from that point on.**

### 2.2 `RJdkX509Intercept` — the OID table holds 1 of 590

```text
AssertionError: RJdkX509Intercept: getSigAlgName() = 1.2.840.113549.1.1.11
```

`getSigAlgName()` returning the OID *is* the JDK's documented fallback for a
name it cannot resolve. Read directly, through
`--add-opens=java.base/sun.security.util=ALL-UNNAMED`:

| | HotSpot | CratonVM, armed |
|---|---|---|
| `KnownOIDs.name2enum.getClass()` | `ConcurrentHashMap` | `ConcurrentHashMap` |
| `KnownOIDs.name2enum.size()` | **590** | **1** |
| `KnownOIDs.values().length` | 280 | 280 |
| `findMatch("1.2.840.113549.1.1.11")` | `SHA256withRSA` | **null** |
| `findMatch("SHA256WITHRSA")` | `SHA256withRSA` | **null** |
| `findMatch("1.2.840.113549.1.1.1")` / `("RSA")` | `RSA` | **null** |
| `findMatch("2.16.840.1.101.3.4.2.1")` / `("SHA-256")` | `SHA_256` | **null** |

280 enum constants register 590 keys (OID + std name + aliases). One survives.

**And the JDK's own guard against exactly this cannot fire.**
`KnownOIDs.register` is written as

```java
KnownOIDs ov = name2enum.put(o.oid, o);
if (ov != null) throw new RuntimeException("ERROR: Duplicate " + o.oid + …);
```

A dropped `put` returns `null`, which is the success value. The class
initialiser completes cleanly with a 1-entry table, and the first thing anybody
learns about it is a certificate that prints an OID where a name belongs.

### 2.3 `RJdkSecurity` — and a native that threw the evidence away

```text
InvalidKeySpecException: cannot generate a usable RSA private key from the given KeySpec
  at RJdkSecurity.signatures(RJdkSecurity.java:583)
```

Line 583 is `kf.generatePrivate(new PKCS8EncodedKeySpec(kp.getPrivate().getEncoded()))`.
Reproduced standalone: `generatePublic` **succeeds**, `generatePrivate` throws —
and the throwable has **no cause and not one JDK frame**:

```text
[0] java.security.spec.InvalidKeySpecException: cannot generate a usable RSA private key from the given KeySpec
      at java.security.GeneralSecurityException.<init>(GeneralSecurityException.java:58)
      at java.security.spec.InvalidKeySpecException.<init>(InvalidKeySpecException.java:63)
      at SecProbe.main(SecProbe.java:33)
```

That string is not in the JDK. It is `native-builtins/src/jca/key_factory.rs`'s
`kf_generate_private`, which drives the **real** `sun.security.rsa` SPI and, on
failure, discards it:

```rust
if let Ok(r) = drive_real_rsa_keyfactory(ctx, *spec, "engineGeneratePrivate", …) { … }
// … and, at the end:
Err(throw_invalid_key_spec(ctx, &format!(
    "cannot generate a usable {} private key from the given KeySpec", algo_name(algo))))
```

**This is not a line somebody forgot** — `drive_real_rsa_keyfactory`'s own doc
says *"the caller falls back to `InvalidKeySpecException` if this returns an
error, so behaviour is no worse than the synthetic fail-closed path"*. That
sentence is true about the OUTCOME and false about the DIAGNOSIS: HotSpot's own
`RSAKeyFactory` chains (`throw new InvalidKeySpecException(e)`), and the cause is
the only thing that would have named §2.2 from this stack.

**ARGUED, not measured:** the real SPI fails here *because of* §2.2 —
`RSAUtil`/`AlgorithmId` resolve `"RSA"` through `KnownOIDs.findMatch`, which
§2.2 measures returning `null`. **Falsifier:** chain the cause (`H13-2` N1) and
re-run; if the cause is not a `NoSuchAlgorithmException` naming RSA, this
paragraph is wrong. I could not measure it because the native deletes the
evidence, which is the point of N1.

## 3. Mechanism C, measured

One `ConcurrentHashMap`, one `putIfAbsent("only","1")`, then the same six
questions asked through the **virtual** door (`ConcurrentHashMap x`) and the
**interface** door (`Map x`):

| question | HotSpot (both doors) | CV virtual | CV interface |
|---|---|---|---|
| `size()` | 1 | 1 | 1 |
| `isEmpty()` | false | false | false |
| `get("only")` | `1` | `1` | `1` |
| **`containsKey("only")`** | true | **true** | **false** |
| **`keySet()`** | `[only]` | **`[only]`** | **`[]`** |
| **`entrySet().size()`** | 1 | **1** | **0** |

and a `put` performed **through the interface door** stores nothing that either
door can then see (`size()==0`, `get()==null`, both doors).

A second, independent axis — the same map, String keys, nothing but `put`s:

| shape | HotSpot | CratonVM, armed |
|---|---|---|
| 3000 `put`s, then 3000 `get`s | size 3000, 3000 hits | **size 1, 1 hit (the first)** |
| 4 `put`s back to back | 4 | **1** |
| the same 4 with `m.size()` between | 4 | **4** |
| the same 4 with `m.get(...)` between | 4 | **4** |
| the same 4 with `new Object()` between | 4 | **4** |
| the same 4 with `Math.abs(1)` between | 4 | **4** |
| the same 4 in a `for` loop | 4 | **4** |
| the same 4 with the `put` return value consumed | 4 | **4** |
| 64 `Integer` keys / 10 `Long` / 10 identity `Object` / 6 enum | all | **all** |
| `put("x","1"); put("x","2")` | returns `1`, `get`→`2` | **correct** |
| `remove("x")` | returns `2`, entry gone | **returns `null`, entry stays** |

So: **String keys collapse and boxed/identity keys do not; a run of `put`s with
nothing between them collapses and the same `put`s with any intervening bytecode
do not.** The two axes are not yet one rule — §5 says so plainly.

## 4. What this is NOT — four negatives, each measured

* **Not mechanism A.** Nothing in these three vectors reads
  `Cannot read field … because "this.this$0" is null`. (CHM's views are *static*
  nested classes with a `map` field, not inner classes with `this$0` — and when
  a CHM view does break, §3's `EntrySetView.iterator()` NPE names `"m" is null`,
  which is the same species under a different field name. Related, not equal.)
* **Not mechanism B.** No `cratonvm.synthetic.AnonymousObject$N` appears in any
  of the three. It appears under `java/util/HashMap` — see §5.
* **Not the JIT.** Every §3 row is byte-identical with `--nojit`. The collection
  direct-bind door (`H7-1`, and the two `jit_*_direct` fixes in this branch's
  gap) is not what is happening here.
* **Not a partially-armed prefix.** §1: 18 CHM rows, all `bytecode-won`.

## 5. Two corrections to records in my own gap

**`H0-4` §4's explanation of `RMapGcStress` does not survive a `HashMap`
control.** It reads the family's `iterated 1 != 3000` as *"the inserts went to a
CratonVM side structure and the iteration reads the real `table`"*. Measured,
with `CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/HashMap` armed:

```text
CK D 3000str size=3000 hits=3000        <- writes AND reads are correct
CK D iterated=  ...ClassCastException: class cratonvm.synthetic.AnonymousObject$4
                   cannot be cast to class java.util.Map$Entry
```

`HashMap` stores all 3000 and retrieves all 3000 through real bytecode. **The
inserts did not go to a side structure.** What breaks is `entrySet()`
iteration, and it breaks as **mechanism B**. `H0-4` §4's *count* (one defect,
four faces) may well still hold; its *cause* does not, and the difference
matters because it moves the repair from "populate the table" to "stop minting
`AnonymousObject$N` as a `Map.Entry`".

**`H0-3` §3's phrasing is right and its emphasis is off.** "Crypto provider
chains … are all built on a map whose contents the VM owns" reads as a
containment problem. Measured, it is a **consistency** problem: the contents are
mostly there, and the accessors disagree about them. That is worse, because
containment fails loudly and inconsistency does not — §2.2's dropped `put` walks
straight through the JDK's own duplicate check.

## 6. What I did NOT verify

* **The VM-side cause of §3.** I did not open
  `native-collections/src/lib.rs` or the interpreter's dispatch to find why the
  interface door and the virtual door disagree, or why an intervening bytecode
  makes a `put` stick. Those files are not this lane's and I would have been
  guessing. §3 is a set of observations with a control, not a diagnosis.
* **Whether §3's two axes are one defect.** The door axis reproduces with a
  single `put`; the sequence axis reproduces with a single door. Nothing here
  reduces one to the other, and this directory's standing error is exactly that
  step.
* **§2.1's `isEmpty()`/`size()` split against §3's simple case.** In §3 the
  virtual `isEmpty()` answered *correctly* (`false`) for a one-entry map, and in
  §2.1 it answered `true`. The difference is that §2.1's `putIfAbsent` was
  issued by real JDK bytecode inside `CryptoPermissions.add` and §3's by
  application bytecode. **That is a hypothesis, not a measurement.**
* **§2.3's link to §2.2.** ARGUED, with its falsifier stated in place.
* **Anything about compatible mode.** The dial only makes contract §1.4
  enforced under `--jdk-only`.
* **The other five vectors of `H0-3`'s eleven** (`RJdkLogging`, `RJdkModule`,
  `RJdkProxyIface`, `RJdkEnumerations`, `RServiceLoaderDoubleSource`) — not this
  lane's, not run.

## 7. NOMINATIONS

* **N1 — the `Map` door and the `ConcurrentHashMap` door are two
  implementations of one instance; find the second one.** §3's table is the
  sharpest single instrument this directory has for the collection migration:
  it needs no suite, no build, and it fails in six lines. Whoever owns
  `native-collections` should run it first and last. *(`C13-1` and `C7-2`
  already named "the interface doors" as a family; this is that family, on the
  class `H0-3` calls the floor.)*
* **N2 — put §3's probe in the arm as a vector.** Six `CK` lines, one map, one
  `putIfAbsent`. It would have caught this at any point in the last year, and
  today it exists because somebody typed it.
* **N3 — re-run `H0-4`'s N1 with §5's control.** If `RMapGcStress` is mechanism
  B rather than "an empty table", the blast-radius table re-prices again and the
  cheap end of the migration (`HashSet`, `Hashtable`) may be cheaper still.
* **N4 — `KnownOIDs.name2enum` is a one-line health check for the whole
  security stack.** `size()` is 590 on a correct JDK 25 and 1 here. Any lane
  touching maps should print it; it is a scalar that cannot be argued with.
* **N5 — chain the cause in `kf_generate_private`.** See `H13-2` N1. Until it is
  chained, §2.3 cannot be measured by anybody, and the next lane will re-derive
  this section from scratch.

---

## INDEPENDENT REPRODUCTION (lane H0, 2026-08-20) — and one detail sharper than reported

Probe written without reading this lane's fixture, on the binary at `fe59bf9d9`.
Saved as `regression-suite/probes/ChmConsistencyProbe.java`.

**Unarmed control: every row correct.** All of the below is dial-caused.

| probe | HotSpot | CratonVM `--jdk-only` **armed** CHM |
|---|---|---|
| 4 back-to-back `put`, String keys | `size=4 keys=[k0,k1,k2,k3]` | **`size=1 keys=[]`** |
| same 4 puts with `size()` between them | `size=4 keys=[k0..k3]` | `size=4 keys=[k0..k3]` |
| same 4 puts with `Math.abs(1)` between them | `size=4 keys=[k0..k3]` | `size=4 keys=[k0..k3]` |
| 4 puts, `Integer` keys | `size=4` | `size=4` |
| one `putIfAbsent`, read via `ConcurrentHashMap` | `true / [only] / 1` | `true / [only] / 1` |
| **the same map**, read via `Map` | `true / [only] / 1` | **`false / [] / 0`** |

**Every claim in this record reproduces**, including the one that reads as
impossible: **interposing a call that does nothing to the map — `Math.abs(1)` —
between the puts makes all four persist.**

**The detail this record understates.** It reports the back-to-back case as
"size 1 (the first)". It is worse than that: **`size()` answers `1` while
`keySet()` is EMPTY.** The map contradicts itself *through a single door*, so
this is not only a door-vs-door disagreement — the size counter and the table
disagree with each other. Any consumer that branches on `isEmpty()` versus
`size()` gets two different answers about the same map, which is exactly the
`CryptoPermissions.isEmpty()` path §4 traces into a dead JCE.

**Why this matters for the plan more than mechanisms A and B do.** A and B fail
*loudly* — an NPE, an `ArrayStoreException`. This one returns wrong answers
quietly and inconsistently, and its trigger is **the instruction sequence around
the put**, not the data. A vector can pass, be edited in a way that changes
nothing semantically, and start failing. Two consequences worth stating:

* **`--nojit` does not change it** (this record measured that, and I did not
  re-check it), so the sequence dependence is not tier-up. Whatever caches or
  defers the put lives below the JIT.
* **String keys only.** `Integer` keys are correct at the same sites. So the
  trigger involves key hashing or interning, which narrows the search a great
  deal and is not stated as a lead anywhere yet.
