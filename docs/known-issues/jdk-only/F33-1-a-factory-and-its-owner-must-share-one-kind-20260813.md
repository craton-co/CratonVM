# F33-1 — a factory and the receiver it hands out must share one kind, and two of the four carriers were never implementable

**2026-08-13, lane F33.** Fixes the `--jdk-only` capability defect F24-1 found
and could not land; deletes `register_java_security_access` (F24-1's N3);
re-verifies `vm/src/runtime/shared_secrets.rs` against the image at `javap -p`
including the one column nobody had checked. All patches are in the working
tree; nothing is committed.

**This lane may not build or run CratonVM, and did not.** Every JDK fact below
is `javap -p` / `java` on this host (Microsoft **25.0.3+9-LTS**, windows/x64) or
the JDK 25 source at `C:\craton\jdk25src`, and is quoted. **Every claim about
CratonVM's behaviour is PREDICTED from source**, and labelled where it matters.
The four edited files were parse-checked (`rustfmt --edition 2021 --emit stdout`
on scratch copies: exit 0, zero `error:` lines, and zero formatting delta in
every region this lane wrote). None was type-checked.

---

## 0. Verdict on the brief

| claim as handed to this lane | verdict |
|---|---|
| `register()` re-tags by receiver class, splitting one ambient-`Bridge` registrar | **CONFIRMED at the source** (`native-api/src/registry.rs:5826-5840`); the re-tag arm fires only on `effective_category() == Bridge` and keys on `class_name` (§1) |
| four factories survive `--jdk-only` over owners it drops | **CONFIRMED**, and the four are exactly the `cratonvm/internal/ss/…$1` rows (§1) |
| `alloc_singleton`'s doc asserts the premise the re-tag invalidates | **CONFIRMED, quoted and corrected** (§1.2) |
| the F24-1 ratchet "will go red; that is the point" | **WRONG, and this is the most useful correction in the lane.** It stays GREEN across the fix — the fix does not change the orphan set it froze (§4) |
| `stub_ratchet.rs:325` is prose, not an assertion | **CONFIRMED** (§3.2) |
| `no_image_receiver.rs` (the `AccessController$1` row) must NOT be removed | **CONFIRMED, and the ORDER is the load-bearing part** (§3.3) |
| deleting `register_java_security_access` reddens the stub ratchet by −2 | **WRONG.** Every ratchet assertion is one-sided (`<=`) or `== 0`; a *decrease* reddens none of them (§3.4) |
| — | **NEW: two of the four fabricated carriers implement NO method their interface declares, in ANY mode** (§2). This decides the disposition |
| — | **NEW: the owner-class ↔ interface pairing in `shared_secrets.rs` had never been checked. It is now, for all eleven JDK-namespaced owners** (§5) |

---

## 1. The defect, re-derived from source before touching it

`register_wp1_4_shared_secrets` sets **one** ambient kind for the whole
registrar (`shared_secrets_bridge.rs`):

```rust
registry.set_category(cratonvm_native_api::NativeKind::Bridge);
```

`NativeMethodRegistry::register` then re-tags by RECEIVER CLASS
(`native-api/src/registry.rs:5826-5840`):

```rust
if self.effective_category() == NativeKind::Bridge
    && (crate::no_image_receiver::receiver_declared_by_no_supported_image(class_name)
        || crate::retired_shadow::triple_is_retired_shadow(class_name, method_name, descriptor))
{ … self.current_category = Some(NativeKind::SyntheticStub); … }
```

The re-tag's argument is `class_name` — **the class the registration is made
on**. For a *factory* that is `SharedSecrets`, not the object the factory
returns. So the one registrar splits:

| registered on | in a `no_image_receiver` table? | kind | `--jdk-only` |
|---|---|---|---|
| `jdk/internal/access/SharedSecrets` — all 14 factories | no; real JDK class | `Bridge` | **survived** |
| `cratonvm/internal/ss/…$1` — four owners' methods | yes, `VM_MINTED_STAND_IN_RECEIVERS` | `SyntheticStub` | **dropped** |

`jdk/internal/misc/SharedSecrets` **is** in `NO_IMAGE_JDK_RECEIVERS` and the
`jdk/internal/access/` spelling is not — correctly, only the legacy alias is
absent from the image — which is why the factory half escaped the re-tag.
`triple_is_retired_shadow` cannot catch it either: it short-circuits on a
`java/util/` prefix (`retired_shadow.rs:546`).

### 1.1 Why it is silent

`gen_factory!` calls `alloc_singleton(ctx, owner)`, which is **infallible**:

```rust
match ctx.ensure_class_initialized(owner_class) {
    Ok(cid) => { let n = ctx.class_num_total_fields(cid); ctx.alloc_object(cid, n.max(1)) }
    Err(_)  => ctx.alloc_object(cratonvm_types::ClassId::new(0), 1)
}
```

So `--jdk-only` code calling `SharedSecrets.javaUtilJarAccess()` got an object of
`ClassId(0)` **typed as a `JavaUtilJarAccess`** — a wrong-class receiver, not an
error, not a log line. (`ensure_synthetic_class` was deleted 2026-08-10, which is
what made the `Err` arm reachable here; before that the other arm fabricated.)

### 1.2 The doc comment that asserted the premise

Verbatim, from the `Err` arm as it stood:

> Class did not exist; use ClassId(0) as a placeholder — the invokeinterface
> resolution still routes through the stored class name in the native registry.

That is a claim about the registry, and the registry is precisely what strict
mode edits. Corrected in place, with the scope of the correction stated: the
premise is now restored *for the `SharedSecrets` factories* by the fix below,
and is deliberately **not** restored for `alloc_singleton`'s other two callers
(`jdk/internal/reflect/ReflectionFactory`, `java/lang/management/BufferPoolMXBean`),
which are real class names taking the arm as a not-loadable fallback. A guard
scoped by a stated premise is only as good as the premise — so the premise is
now written down next to its exceptions instead of implied.

---

## 2. The measurement that decided the disposition

The brief offered three honest options: retarget the carriers onto real JDK
classes, tag the factories to match their owners, or drop the factories so
strict mode refuses cleanly. **The choice is not a preference here — one option
is refuted by measurement.** `javap -p` on 25.0.3+9-LTS, each carrier's
registered methods against the interface its factory's return descriptor
promises:

```text
  JavaIORandomAccessFileAccess   registered  open(String,String), openAsChannel(RandomAccessFile)
                                 JDK 25      openAndDelete(File,String)            0 of 1
  JavaNetHttpCookieAccess        registered  parseCookie(String)
                                 JDK 25      parse(String), header(HttpCookie)     0 of 2
  JavaUtilJarAccess              registered  jarFileHasClassPathAttribute, ensureInitialization
                                 JDK 25      + getTrustedAttributes, isInitializing, entryFor
                                                                                   2 of 5
  JavaNetUriAccess               registered  create(String,String)                 1 of 1
```

`openAndDelete` is the **sole** method on `JavaIORandomAccessFileAccess`,
confirmed twice — `javap -p` and
`jdk25src/java.base/jdk/internal/access/JavaIORandomAccessFileAccess.java`.
`parseCookie` and `open` are names CratonVM invented; `grep -rn` over `*.rs` /
`*.java` finds them only in this bridge and in the tables that list its owners.

So **two of the four carriers cannot service a single `invokeinterface` against
their declared interface, in `--real-jdk` either.** "Make them work" is not a
tag change; it is writing two implementations against interfaces nobody had
read. That is a real piece of work and it is not this defect.

Retargeting onto the JDK's own implementation classes is the other real option
and is written up as a nomination rather than taken (§6, N2): those classes all
exist —

```text
  java.util.jar.JavaUtilJarAccessImpl implements jdk.internal.access.JavaUtilJarAccess
  java.net.URI$1                      implements jdk.internal.access.JavaNetUriAccess
```

— but registering natives on them makes CratonVM shadow live bytecode on real
classes in **both** modes, which is a §1.4 question and needs a strict-corpus
measurement this lane cannot take.

Refusing in strict needs no measurement, and that is the argument for it: the
thing being refused is *already* broken there, by construction. Nothing that
works today stops working.

### 2.1 What the real JDK does instead, quoted

All four accessors are self-initialising
(`jdk25src/java.base/jdk/internal/access/SharedSecrets.java`):

```java
public static JavaNetUriAccess getJavaNetUriAccess() {
    var access = javaNetUriAccess;
    if (access == null) { ensureClassInitialized(java.net.URI.class); access = javaNetUriAccess; }
    return access;
}
```

with `URI.<clinit>` doing `SharedSecrets.setJavaNetUriAccess(new JavaNetUriAccess() { … })`
(`URI.java:3730`), and the same shape for `JarFile` (`:167`), `HttpCookie`
(`:989`) and `RandomAccessFile` (`:1272`). So the refusal hands the call to code
that knows how to build its own answer. **PREDICTED, not measured:** whether
CratonVM can run those four `<clinit>`s is the open risk (§7).

---

## 3. What was done

### 3.1 The fix — a derived rule, not a second list

`register_factories` now asks the *owner* the question the re-tag asks the
receiver:

```rust
fn factory_is_orphaned_by_strict_mode(owner_class: &str) -> bool {
    cratonvm_native_api::no_image_receiver::receiver_declared_by_no_supported_image(owner_class)
}
```

and registers those factories with `register_with_kind(…, SyntheticStub)`. The
other ten keep plain `register` and inherit the ambient `Bridge` — **only the
demotion is stated**, so the change cannot move a row it is not about, and a
future re-scoping of the registrar still governs the ten.

`register_with_kind` also sets `kind_stated`, which is the right provenance: a
measurement adjudicated this row, not an author's ambient scope.

Why derived rather than a hand-written list of four: F24-1's whole finding was
two hand-maintained copies of one table drifting by a swap that every length
check on both sides passed. A second list here would be the same shape.

### 3.2 `register_java_security_access` — deleted (F24-1's N3)

Three facts, each measured on 25.0.3+9-LTS:

```text
$ javap -p jdk.internal.access.JavaSecurityAccess   -> class not found
$ javap -p 'java.security.AccessController$1'       -> class not found
$ javap -p jdk.internal.access.SharedSecrets | grep -c getJavaSecurityAccess  -> 0
```

JEP 486 took the interface, and the implementation class and the accessor with
it. **Reachability re-verified before deleting**, because `call_native` panics on
an unregistered triple and a Rust panic is not a Java throwable — it kills the
VM. Three doors, all closed: no JDK 25 bytecode can name a receiver type its own
image does not declare; `grep -rn 'AccessController[$]1'` over `*.rs`/`*.java`
finds no mint site (`ensure_class_initialized` / `new_object` / `alloc_object`)
— the only one was the `f_jsec` factory callback, unreachable since F17-1 removed
its `FACTORIES` row; and `owner_classes()` iterates `FACTORIES`, which has had no
`java/security/` entry since then.

Deleted together: the two bodies, the registrar, its call site, the dead
`gen_factory!(f_jsec, …)` and its match arm, and the
`representative_method_registered_per_owner` probe (15 → 14). **That probe is
why two earlier lanes could not land this**: it names the triple, so registrar
and probe must move in one commit.

`stub_ratchet.rs:325`'s mention is **prose inside a doc comment** — the
"923 → 939" narrative listing sixteen restored rows. Read directly: it is inside
a `///` block with no `assert` in it. F24-1's correction of F17-1 is confirmed.
Annotated rather than rewritten; a recount narrative edited to match a later
tree stops being checkable.

### 3.3 `no_image_receiver.rs`'s `AccessController$1` row — kept, and the order is the point

Confirmed: that row was the **only** thing tagging the two `AccessController$1`
natives `SyntheticStub`. Deleting it while the registrations existed would have
promoted two fabricated natives to `Bridge` and admitted them to `--jdk-only` —
the `java.util.Properties` inversion. It is kept, is now **inert**, and the doc
says both, with the ordering constraint spelled out for the next reader. The
image fact is true and re-measured, so the gate script should keep checking it.

### 3.4 The stub ratchet does NOT go red on the deletion

The brief (via F24-1 §3) predicted the −2 reddens the gate. It does not. Every
assertion in `native-builtins/tests/stub_ratchet.rs` is one-sided or a zero:

* `synthetic_stub_count_does_not_regress` — `synthetic <= BASELINE_SYNTHETIC_STUBS`
* `strict_registry_has_zero_synthetic_stubs` — `strict_stubs == 0`
* `strict_registry_drops_only_the_stubs` — `strict_total <= compat_total`, `dropped >= compat_stubs`

A **decrease** reddens none of them. What can redden the first is the **+4** from
§3.1. Net **+2**, and the gate is *already* firing for unrelated, attributed
reasons (1253 frozen vs 1261 observed, no-management; W7-30 §11, W7-62). So the
protocol that file states — *"anything the run reports beyond (a) is a finding to
attribute, not slack to absorb"* — is followed: a cause **(d)** is added beside
(a), (b), (c), with both halves attributed and the predicted totals written down
(**1263** no-management, **1273** management, both derived from the observed
1261/1271, not from the stale frozen 1253/1263). **The constant is NOT
re-frozen.** Raising a slack-free baseline from a derivation rather than from the
printed recount line is the one edit that can do damage.

---

## 4. The F24-1 ratchet stays GREEN across the fix — and that is why it had to be replaced

The brief says to expect
`strict_keeps_no_shared_secrets_factory_whose_owner_it_dropped` to go red. **It
does not, and the reason is worth more than the test.**

That test freezes the **orphan set**: the owner classes strict mode leaves with
no registered method. Refusing the four factories does not add a method to any
of those four owners — they are exactly as method-less after the fix as before.
So the frozen set is unchanged and the assertion passes, silently, across the
repair it was written to notice.

It is the same defect species F24-1 itself documented one level up: a guard that
measures the wrong projection of the thing it is about. The orphan set is a fact
about **owners**; the defect is a fact about the **pairing**.

Replaced by `no_shared_secrets_factory_outlives_the_owner_strict_mode_drops`,
which asserts the pairing directly — for every `FACTORIES` row, "the factory
survives strict" must equal "its owner survives strict" — over the boot registry
in `CompatibilityMode::JdkOnly`. Both of F24-1's directions are preserved (growth
reddens; a real fix must shrink a named list deliberately) and the check now
actually moves when the defect does.

F24-1's premise assertion is kept verbatim: this ratchet has had a scope hole
twice, and `register_wp1_4_shared_secrets` reaches `register_boot_path` only
transitively through `register_essential_natives_with_shims`
(`native-builtins/src/lib.rs:10063`), so the "SharedSecrets is live" assertion
stays or the whole test could pass on an empty set.

To read the pairing from outside the crate, `factory_methods_and_owners()` was
added next to `owner_classes()` — `owner_classes()` projects away exactly the
column the invariant needs. A test-local copy of fourteen name/owner pairs was
the alternative, and a second copy of this table is how the lists drifted in the
first place.

### 4.1 Mutation check, both directions, for all three new guards

Reasoned against the source, **not run** (this lane may not invoke `cargo`).
Stated per guard so a reader can falsify each claim independently:

| guard | mutation | killed? |
|---|---|---|
| `factory_kind_follows_the_owner_it_hands_out` | revert §3.1 (all 14 plain `register`) | YES — 4 rows come back `Bridge` while their owner is dropped; the `==` fails on the first |
| ″ | "fix" by demoting **every** factory | YES — the 10 real-JDK owners fail the `==` in the other direction |
| ″ | add a 15th factory over a stand-in owner without demoting it | YES — same `==` |
| ″ | retarget a carrier onto a real class and forget the frozen list | YES — the second `assert_eq!` on the four names |
| `every_fabricated_factory_owner_is_a_listed_stand_in` | add owner `cratonvm/internal/ss/Foo$1`, omit the `VM_MINTED_STAND_IN_RECEIVERS` entry | YES — and this is the mutation the IFF guard **cannot** kill: unlisted, factory and owner would agree (both `Bridge`, both surviving), so the IFF is satisfied by the wrong shared answer. That is why there are two guards |
| `no_shared_secrets_factory_outlives_the_owner_strict_mode_drops` | revert §3.1 | YES — 4 mismatches |
| ″ | a second registrar re-registers a refused factory under a `Bridge` scope | YES — this is the case the unit test cannot see, and the reason the rule is stated at two scopes |
| ″ | boot path stops reaching the registrar | YES — the `live_classes.contains("jdk/internal/access/SharedSecrets")` premise assertion |
| — | **control:** the guard this replaced, against the mutation that is the fix | **SURVIVES.** `strict_keeps_no_shared_secrets_factory_whose_owner_it_dropped` is green both before and after §3.1. It did not merely fail to help; it certified the defect |

The control is the load-bearing row, as it was in F24-1: the replaced guard is
insensitive to the exact change it was written for.

---

## 5. Task 3 — `vm/src/runtime/shared_secrets.rs`, re-verified and one column added

**F24-1's state is confirmed by independent re-measurement**, not by its
self-report. All fifteen `factory_method()` spellings, swept against
`javap -p jdk.internal.access.SharedSecrets`: **exactly one match each**, with
the declared return type equal to the variant's `interface_class()`. Negative
controls `getJavaSecurityAccess`, `getJavaUtilJarAccess` and a nonsense name all
score **0**. `JavaIOFileDescriptor` is present and correct. Both spelling fixes
are in the tree.

`javap -p` is the only admissible instrument on this class: it reports **98**
members (32 fields + 66 methods) where plain `javap` reports 65. The one
*method* the public view hides is `ensureClassInitialized` — which is exactly the
machinery the four self-initialising getters of §2.1 run through, i.e. the public
view hides the evidence this lane's disposition turns on.

**The column nobody had checked** is `owner_class()`. Every prior lane verified
that the owner class *exists*; none verified that it **implements the interface
the variant names** — a different and stronger claim, and one that is easy to get
right by luck, since `java.nio.Buffer$1` is the IOOBE formatter and `Buffer$2` is
the access impl (a mistake already made once in this file). `javap -p` prints the
`implements` clause, so it is readable rather than inferred. All eleven
JDK-namespaced owners:

```text
  java.lang.System$1                   implements JavaLangAccess               OK
  java.lang.invoke.MethodHandleImpl$1  implements JavaLangInvokeAccess         OK
  java.lang.ref.Reference$1            implements JavaLangRefAccess            OK
  java.lang.reflect.ReflectAccess      implements JavaLangReflectAccess        OK
  java.io.Console$1                    implements JavaIOAccess                 OK
  java.io.FileDescriptor$1             implements JavaIOFileDescriptorAccess   OK
  java.net.InetAddress$1               implements JavaNetInetAddressAccess     OK
  java.nio.Buffer$2                    implements JavaNioAccess                OK
  java.util.zip.ZipFile$1              implements JavaUtilZipFileAccess        OK
  java.util.ResourceBundle$1           implements JavaUtilResourceBundleAccess OK
  java.io.ObjectInputStream$1          class not found                         ABSENT
```

Ten of eleven pair correctly; the eleventh is the documented `invokedynamic`
asymmetry. Run with a negative control (`java.lang.NoSuchClassAtAllXyz` →
`class not found`) so "absent" is distinguishable from "javap cannot see the
module"; all fifteen `Java*Access` interfaces are PRESENT on the same image,
which is the positive control for the same question. Without those controls a
sweep of this shape reports its own reach.

Recorded on `owner_class`'s doc comment, with the four `cratonvm/` names' §2
table beside it — they cannot be checked this way, and that is the finding
rather than a gap in the sweep.

**Baseline arithmetic, re-checked because F24-1's own note said it had moved
underneath it:** `scripts/baselines/jdk25-jdk.internal.access.SharedSecrets.tsv`
carries **101** data rows = 1 CLASS + 1 EXTENDS + 1 SUPERTYPE + 32 FIELD + 66
METHOD, with 33 `private,static` rows. That agrees with `javap -p`'s 98 members
and is now an *independent* corroboration, since the public-only filter was fixed
(F23-1). Which makes a doc comment in `shared_secrets_bridge.rs` false — see §6.1.

---

## 6. Nominations, and whether any blocks compilation

**None of these blocks compilation.** The two generated baselines are consumed
only by `scripts/jdk-only-kind-map.py`, `scripts/jdk-only-adjudicate.py` and
`regression-suite/bridge-ratchet.sh` — no Rust test reads either file — and both
gate scripts exit **2** ("REFUSING") off Linux.

### N1 — `scripts/baselines/jdk-only-gated-never-delete.tsv`: two stale rows

Lines 86-87 name the two deleted `AccessController$1` triples. **Regenerate; do
not hand-edit.** Their `registered_by` column already points at
`shared_secrets_bridge.rs:2446`, which was stale before this lane touched
anything (the function was at `:2649`), so the file is demonstrably drifting from
the tree and a hand edit would hide that. For the record, the exact rows to
disappear on the next regeneration:

**old**
```
java/security/AccessController$1	doIntersectionPrivilege	(Ljava/security/PrivilegedAction;Ljava/security/AccessControlContext;Ljava/security/AccessControlContext;)Ljava/lang/Object;	synthetic-stub	native-builtins/src/shared_secrets_bridge.rs:2446	kind=synthetic-stub
java/security/AccessController$1	getProtectDomains	(Ljava/security/AccessControlContext;)[Ljava/security/ProtectionDomain;	synthetic-stub	native-builtins/src/shared_secrets_bridge.rs:2452	kind=synthetic-stub
```
**new** — both lines absent.

### N2 — `scripts/baselines/jdk-only-kind-map-25-linux.tsv`: two deletions, four kind changes

Lines 5790-5791 (the same two triples) go. Four rows change kind
`bridge` → `synthetic-stub`:

```
jdk/internal/access/SharedSecrets	javaUtilJarAccess	()Ljdk/internal/access/JavaUtilJarAccess;
jdk/internal/access/SharedSecrets	getJavaNetUriAccess	()Ljdk/internal/access/JavaNetUriAccess;
jdk/internal/access/SharedSecrets	getJavaNetHttpCookieAccess	()Ljdk/internal/access/JavaNetHttpCookieAccess;
jdk/internal/access/SharedSecrets	getJavaIORandomAccessFileAccess	()Ljdk/internal/access/JavaIORandomAccessFileAccess;
```

**Regenerate from ONE Linux census**, together with
`scripts/baselines/jdk-only-bridge-ratchet.json`, via
`sh regression-suite/bridge-ratchet.sh --update-baseline --note "F33-1: …"` —
they are two readings of one measurement. A frozen census row edited by hand is
a claim about a tree nobody censused.

### N3 — `native-builtins/src/lib.rs:4527-4530`: a header naming a deleted getter

F24-1's N1 and N2 were **both still unapplied** — verified by grep against the
current tree, not taken from either record's self-report. N1 (the "compile-time
`#[test]`" claim above `FACTORIES`) is in a file this lane owns, so it was
**applied here** rather than re-nominated; F24-1 could only nominate it. N2 is in
`lib.rs`, which this lane does not own, and it is now wrong in a second way — the
comment says "15" and `FACTORIES` has held 14 since F17-1:

**old**
```
// WP1.4 — `jdk.internal.access.SharedSecrets` bridge: 15 *Access
// interface singletons + every per-interface method.  Unblocks
// callers that look up `SharedSecrets.getJavaLangAccess()` /
// `getJavaSecurityAccess()` / etc. during boot and invokeinterface
```
**new**
```
// WP1.4 — `jdk.internal.access.SharedSecrets` bridge: 14 *Access
// interface singletons + every per-interface method.  Unblocks
// callers that look up `SharedSecrets.getJavaLangAccess()` /
// `javaUtilJarAccess()` / etc. during boot and invokeinterface
```
(`getJavaSecurityAccess` was deleted by F17-1 — JEP 486 removed the interface —
and `javaUtilJarAccess` is the JDK's spelling, with no `get` prefix. Four of the
14 are refused under `--jdk-only` as of F33-1, which the comment need not say.)

### N4 — retarget the two unimplementable carriers (the real fix behind §2)

Not this lane's, and not landable without a strict-corpus run. `JavaNetUriAccess`
and `JavaUtilJarAccess` have real implementation classes on the image
(`java/net/URI$1`, `java/util/jar/JavaUtilJarAccessImpl`);
`JavaNetHttpCookieAccess` and `JavaIORandomAccessFileAccess` are anonymous
classes minted in `HttpCookie.<clinit>` / `RandomAccessFile.<clinit>` whose `$N`
index must be read off the image, **not guessed** — `java.net.HttpCookie$1`
implements `CookieAttributeAssignor`, and `java.io.RandomAccessFile$1` implements
`Closeable`, so in both cases the obvious `$1` is the WRONG class. Retargeting
makes the natives shadow live bytecode in both modes; that is a §1.4 retirement
question, and `retired_shadow.rs`'s header states the order it must follow.

### N5 — generate JDK baselines for the four `Java*Access` interfaces

`scripts/baselines/` has a TSV for `jdk.internal.access.SharedSecrets` but for
none of the interfaces it hands out, which is why §2's table is a `javap`
transcript in a doc comment rather than an assertion. Generating
`jdk25-jdk.internal.access.JavaUtilJarAccess.tsv` and its three siblings with
`scripts/jdk-baseline/generate.py` would let a guard check *registered methods vs
declared methods* per owner — the check that would have caught `parseCookie` and
`open` at the moment they were invented. `scripts/jdk-baseline/` is another
lane's; nominated, not attempted.

### N6 — `java/io/ObjectInputStream$1` → `NO_IMAGE_JDK_RECEIVERS`

F24-1's N4, unchanged and still blocked on the same thing: this table's threshold
is all six images and only 25.0.3 is on this host. Re-measured here (ABSENT, §5).
Now slightly more attractive than when F24-1 wrote it, because §3.1 means adding
it would carry no factory with it — `register_java_object_input_stream_access`'s
two rows are the whole exposure, and there is no factory for that owner. Still a
one-image add, so still held.

---

## 7. What each mode does after the change — verified vs. assumed

| mode | after | basis |
|---|---|---|
| `--real-jdk` / `Compatible` (the default) | **unchanged, for every one of the 14 factories.** A `SyntheticStub` registers and dispatches normally in `Compatible`: `register_inner`'s `JdkOnly` arm is gated on `compatibility_mode == JdkOnly`, and the `drop_synthetic_stubs` arm on the `CRATONVM_NO_STUBS` env var | **VERIFIED at the source** (`registry.rs:5871` and `:5904`). No behavioural claim, no measurement needed |
| `--jdk-only` | the four accessors over fabricated owners are **refused at registration**, each recording a `JdkOnlyViolation::SyntheticNativeRegistered` with provenance, visible in `--jdk-only-report` and under `CRATONVM_DBG_DROPPED_STUBS=1`. The call then resolves to real JDK bytecode | **PREDICTED.** The refusal path is verified at the source; that the JDK's `<clinit>` chain then produces a working singleton under CratonVM is NOT |
| `--jdk-only`, the other ten factories | unchanged, `Bridge`, surviving | VERIFIED (only the demotion is stated) |
| `--features synthetic-jdk` | unchanged — the mode is `Compatible` there, so the refusal arm never runs | VERIFIED, same source |
| all modes | two `AccessController$1` natives no longer exist. Nothing could reach them (§3.2) | **VERIFIED** by three independent closures, plus `javap` |

**The honest risk, stated rather than buried.** If CratonVM cannot run
`URI.<clinit>` / `JarFile.<clinit>` / `HttpCookie.<clinit>` /
`RandomAccessFile.<clinit>` to completion under `--jdk-only`, the real getter
returns `null` and a caller NPEs. Today that caller instead receives a
`ClassId(0)` object and `AbstractMethodError`s (or worse, silently misbehaves) on
its first interface call. **A named `NullPointerException` at a real call site is
strictly better than a wrong-class receiver no instrument reports** — but it is a
different failure, and if a strict-corpus vector moves, this is the first place
to look. The first arm to take: `CRATONVM_DBG_DROPPED_STUBS=1 cratonvm --jdk-only`
and grep `[JDK-ONLY-REFUSED] jdk/internal/access/SharedSecrets`.

---

## 8. Which checks flip vs. which become reachable

* **Red → green by this change:** none. Nothing was red *for this reason*.
* **Would have gone red on the corrected tree, so it moved with the fix:**
  `representative_method_registered_per_owner` (it names the deleted
  `AccessController$1` triple; 15 → 14) — the assertion that blocked two earlier
  lanes.
* **Green before AND after the fix, therefore replaced:**
  `strict_keeps_no_shared_secrets_factory_whose_owner_it_dropped` (§4). This is
  the important one.
* **Newly able to fail at all:** `factory_kind_follows_the_owner_it_hands_out`,
  `every_fabricated_factory_owner_is_a_listed_stand_in`,
  `no_shared_secrets_factory_outlives_the_owner_strict_mode_drops`.
* **Expected to move, attributed, NOT re-frozen:**
  `synthetic_stub_count_does_not_regress` — net **+2** (§3.4). Already firing.
* **Predicted-green, unverified:** all of the above. Not compiled, not run.

---

## 9. Left undone, and why

1. **The `BASELINE_SYNTHETIC_STUBS` re-freeze.** Needs a build and a run in each
   feature configuration, from the printed recount line. The +2 is derived and
   labelled as such; pasting a derived number into a slack-free gate is the one
   edit that admits new stubs in silence.
2. **The two generated baselines** (N1, N2). Regeneration needs a Linux census.
3. **The retarget** (N4) — the change that would make the four carriers *work*
   rather than be refused. Needs the `$N` indices read off the image and a
   strict-corpus measurement.
4. **Per-owner interface-surface guards** (N5) — blocked on baselines in another
   lane's directory. §2's table is prose in a doc comment meanwhile, which is
   weaker than an assertion and is why it is quoted with its command.
5. **No type-check of any kind.** `rustfmt` parse only, on scratch copies. The
   most likely first-build failure is the new `pub fn factory_methods_and_owners`
   / `NativeKind::as_str` / `kind_of` call sites in the two new tests — all three
   APIs were read at their definitions, but that the paths resolve from those
   modules is assumed, not verified.
6. **`--features synthetic-jdk` was reasoned about, not built.** The claim is
   that the mode is `Compatible` there so nothing changes; that follows from the
   same source arm as the `--real-jdk` row and inherits its confidence, no more.
