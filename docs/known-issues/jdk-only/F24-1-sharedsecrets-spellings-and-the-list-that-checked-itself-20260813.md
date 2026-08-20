# F24-1 — two lists of fifteen that disagreed in both directions, a sync test that was never written, and a factory strict mode keeps for an owner it drops

**2026-08-13, lane F24.** Finishes the `SharedSecrets` correction F17 could not
complete, in `vm/src/runtime/shared_secrets.rs`; adds the cross-crate check whose
absence let the drift happen; and records a live `--jdk-only` capability defect
found in `native-api/src/no_image_receiver.rs`'s neighbourhood. All patches are
in the working tree; nothing is committed.

**This lane may not build or run CratonVM, and did not.** Every JDK fact below is
`javap -p` / `java` on this host (Microsoft **25.0.3+9-LTS**) or the JDK 25
source at `C:\craton\jdk25src`, and is quoted. **Every claim about CratonVM's
behaviour is PREDICTED from source.** The three edited files were parse-checked
(`rustfmt --edition 2021 --emit stdout` on scratch copies, exit 0, zero `error:`
lines), which rules out syntax errors and nothing else; none was type-checked.

---

## 0. Verdict

| claim as handed to this lane | verdict |
|---|---|
| `shared_secrets.rs` still carries both bad spellings | **CONFIRMED.** `getJavaSecurityAccess` deleted, `getJavaUtilJarAccess` → `javaUtilJarAccess` (§1) |
| the file's "compile-time test that keeps the two lists in sync" claim is false | **CONFIRMED, and the claim is in the OTHER file.** The test now exists (§2) |
| the lists diverged while both were length 15 | **CONFIRMED, and in BOTH directions** — the vm list was also *missing* `JavaIOFileDescriptor`, which is real. Added (§2.1) |
| Task 3 — delete `register_java_security_access` atomically across three owned files | **NOT LANDABLE AS DESCRIBED, and one instruction in it is actively wrong.** The pins span six files; the `no_image_receiver.rs` entry must **stay** (§3) |
| — | **NEW: a `--jdk-only` wrong-receiver defect.** Four SharedSecrets factories survive strict mode and return owners whose every method strict mode dropped (§4) |
| — | **NEW: `java/io/ObjectInputStream$1` is a missing `NO_IMAGE_JDK_RECEIVERS` entry** — nominated, not landed, for a measurement gap (§4.3) |

**Two of the three file paths in the assignment do not exist.** The real ones are
`native-api/src/no_image_receiver.rs` (not `native-builtins/src/`) and
`native-builtins/tests/stub_ratchet.rs` (a test, not `src/`). There is also a
second `vm/tests/stub_ratchet.rs`, untouched.

---

## 1. The two spellings — measured first

`javap -p`, not plain `javap`. The difference is real on this class: plain
`javap` prints 65 members, `-p` prints **98** — 32 fields + 66 methods, the 66
being 65 accessors/mutators plus the constructor. The single *method* plain
`javap` hides is `ensureClassInitialized`; the other 32 hidden members are the
private static backing fields.

So the member count is 98, not 65 and not 30. **An earlier record's "30" was the
getter family alone; "65" is the public-method count.** Neither is the surface.

**Baseline reconciliation, re-checked at the end of this lane's work because it
moved underneath it.** `scripts/baselines/jdk25-jdk.internal.access.SharedSecrets.tsv`
now carries **101 data rows = 1 CLASS + 1 EXTENDS + 1 SUPERTYPE + 32 FIELD + 66
METHOD**, i.e. the 98 members plus three metadata rows, with 33 `private,static`
rows present. That is a change made by another lane *during* this session (see
`F23-1-the-guard-that-could-not-see-a-private-native-20260813.md`): the
public-only blindness that F17 documented at `generate.py:175`, and reasoned
around in `shared_secrets_bridge.rs`'s guard doc, **has been fixed**. Two
consequences worth flagging rather than leaving to be rediscovered:

* F17's note that "its 65 baseline rows are its whole surface" is now stale in
  its arithmetic (the rows are 101/98) though its conclusion still holds — every
  member of `SharedSecrets` really is `static`, and the getters really are all
  `public`.
* The `javap -p` count and the baseline are now **independent and agreeing** on
  98 members. Before the regeneration they were not independent, and quoting one
  as corroboration of the other would have been the proxy-oracle error.

```
$ javap -p jdk.internal.access.JavaSecurityAccess
Error: class not found: jdk.internal.access.JavaSecurityAccess

$ javap -p jdk.internal.access.SharedSecrets | grep -c getJavaSecurityAccess
0

$ javap -p jdk.internal.access.SharedSecrets | grep JarAccess
  private static jdk.internal.access.JavaUtilJarAccess javaUtilJarAccess;
  public static jdk.internal.access.JavaUtilJarAccess javaUtilJarAccess();
  public static void setJavaUtilJarAccess(jdk.internal.access.JavaUtilJarAccess);
```

All 15 factory names in the file were checked this way; 13 were already right.

* **`JavaSecurity`: deleted, whole variant.** JEP 486 removed the Security
  Manager and `jdk.internal.access.JavaSecurityAccess` with it — there is no
  differently-spelled member to correct to. Its `owner_class` was fabricated too:
  `javap -p java.security.AccessController$1` → `class not found`.
  `getJavaxSecurityAccess`, `getJavaSecuritySpecAccess`,
  `getJavaSecuritySignatureAccess` and `getJavaSecurityPropertiesAccess` all
  exist and are near-misses in a name-keyed search; none is a rename of this one.
* **`JavaUtilJar`: renamed** to `javaUtilJarAccess`. The *setter* carries the
  `get`-style prefix (`setJavaUtilJarAccess`), which is how the getter's spelling
  got invented, here and in the bridge.

### 1.1 This file has no runtime consumer, and that is worth stating

```
$ grep -rn 'SharedSecretsInterface\|SHARED_SECRETS_OWNERS\|SharedSecretsRegistry' \
      --include=*.rs . | grep -v vm/src/runtime/shared_secrets.rs
(no output)
```

Nothing outside the file reads any of it. The bridge iterates its own private
`FACTORIES`. So **these spelling fixes change no dispatch and cannot fix a
runtime symptom** — a claim to the contrary would be false. Two of the file's own
doc comments asserted otherwise and are corrected: `all()` claimed it was "used
by the native-builtins registration loop and by the `apps/sharedsecrets_probe`
integration test", and `apps/sharedsecrets_probe` does not exist anywhere in the
tree. What the fixes buy is that the file the bridge names as the canonical list
now agrees with the JDK, and is checked.

---

## 2. The sync claim, and the check that now exists

The false claim is **not** in this lane's file. It is in
`native-builtins/src/shared_secrets_bridge.rs:55-59`, above `FACTORIES`:

> Mirrors `vm/src/runtime/shared_secrets.rs::SharedSecretsInterface` (we can't
> `use` it here because native-builtins does not depend on vm; **a compile-time
> `#[test]` in the vm crate asserts the two lists stay in sync**).

No such test existed. What the two lists actually contained:

| owner class | `all()` (vm) | `FACTORIES` (bridge) |
|---|---|---|
| `java/io/FileDescriptor$1` | **absent** | present |
| `java/io/ObjectInputStream$1` | present | **absent** |

Both lists were length 15. Every guard on either side was a length or a count,
so all of them passed. This is the shape the assignment named, and it is worth
being precise about why a length check is not merely weak but *systematically*
blind here: the two lists drifted by a **swap**, and a swap is invisible to any
cardinality.

### 2.1 One row of that table is now closed

`JavaIOFileDescriptor` was **added** to the vm enum. Unlike the deleted
`JavaSecurity`, all three of its parts are real:

```
$ javap -p jdk.internal.access.JavaIOFileDescriptorAccess
public interface jdk.internal.access.JavaIOFileDescriptorAccess { ... }
$ javap -p 'java.io.FileDescriptor$1'
class java.io.FileDescriptor$1 implements jdk.internal.access.JavaIOFileDescriptorAccess
$ javap -p jdk.internal.access.SharedSecrets | grep getJavaIOFileDescriptorAccess
  public static ...JavaIOFileDescriptorAccess getJavaIOFileDescriptorAccess();
```

The other row is a deliberate asymmetry (JDK 25 builds the
`JavaObjectInputStreamAccess` via `invokedynamic` — `ObjectInputStream.java:4039`,
`setJavaObjectInputStreamAccess(ObjectInputStream::checkArray)` — so there is no
`ObjectInputStream$1` and the bridge intentionally does not intercept the getter)
and is pinned by name rather than papered over.

### 2.2 The crate boundary is real, but not where it blocks this

`native-builtins`'s external JDK-surface oracle `jdk_baseline` — the one F17's
replacement guards use — is `pub(crate)` (`jdk_baseline.rs:89,315,481`), so the
vm crate **cannot** reach it. But `shared_secrets_bridge::owner_classes()` is
`pub` in a `pub mod`, and `vm` already depends on `cratonvm-native-builtins`
(`vm/Cargo.toml`), so the bridge's live owner set **is** observable from a vm
unit test. Owner class is precisely the field the two tables disagreed on, so
that projection is sufficient. Three guards replace four:

| new | replaces | fails when |
|---|---|---|
| `owner_classes_mirror_the_native_builtins_bridge` | *(nothing — this is the missing test)* | any owner-set drift, either direction, except the one documented exception |
| `factory_method_matches_the_jdk25_surface` | `factory_method_matches_canonical_shape` | the `javaUtilJarAccess` exception list empties (someone "restores consistency") **or** grows |
| `owners_slice_equals_all` | `all_contains_exactly_fifteen_entries` | the two copies of the list differ in contents, not just length |

`every_interface_has_distinct_owner_class`'s literal `15` was also derived from
`all().len()`.

### 2.3 Mutation check

Required by the assignment, and run. A harness extracts the **real literals from
both source files** (no hand-copied lists — it parses `owner_class`'s and
`factory_method`'s match arms, `all()`, `SHARED_SECRETS_OWNERS`, and the bridge's
`FACTORIES` owner column), evaluates the three guards as pure functions, then
perturbs each input:

```
ours all()           : 15
SHARED_SECRETS_OWNERS: 15
bridge FACTORIES     : 14

-- baseline (all must PASS) --
  PASS  owners_slice_equals_all
  PASS  factory_method_matches_the_jdk25_surface
  PASS  owner_classes_mirror_the_bridge

-- mutations (all must be KILLED) --
  killed   drop JavaIOFileDescriptor, keep len()==15 (THE historical bug)   [mirror]
  killed   re-add the `get` prefix to javaUtilJarAccess                     [factory_shape]
  killed   re-add the JEP 486 JavaSecurity variant                         [mirror]
  killed   SHARED_SECRETS_OWNERS swaps its last entry (len unchanged)       [owners_equal]
  killed   bridge adds an owner class ours lacks                           [mirror]
  killed   a second non-getJava* factory name appears                      [factory_shape]

-- control: the guard F24-1 REPLACED, against the same mutation --
  *** SURVIVED *** old factory_method_matches_canonical_shape vs `getJavaUtilJarAccess`
  old guard red on corrected tree -> confirms it punished the repair

survivors: 0
```

The control is the load-bearing half: the guard being replaced **survives the
exact mutation that was the real bug**, and **goes red on the corrected tree**.
It did not merely fail to help; it penalised the fix.

**What this does not establish:** the harness is Python evaluating the guards'
logic against the real literals. It is not `cargo test`, and it cannot catch a
Rust type error. Compare `[proxy oracle]` — an exhaustive sweep against a
stand-in is still a sweep against a stand-in. The Rust must still be built.

---

## 3. Task 3 — not landable as described, and one instruction in it is wrong

The assignment states `register_java_security_access` "is atomic across your
other two files". It is not. The function lives in
`native-builtins/src/shared_secrets_bridge.rs:2649` — a file this lane was
explicitly told not to touch — and its name or its two triples are pinned in
**six** places:

| # | site | effect of deleting the registrations |
|---|---|---|
| 1 | `shared_secrets_bridge.rs:2649` fn + bodies `jsec_*`, call site `:2894` | the deletion itself |
| 2 | `shared_secrets_bridge.rs:301,324` — `gen_factory!(f_jsec, …)` + match arm | already dead since F17-1; likely `dead_code` warnings |
| 3 | `shared_secrets_bridge.rs:3103` — `representative_method_registered_per_owner` **probes `getProtectDomains` by name** | test goes RED |
| 4 | `native-builtins/tests/stub_ratchet.rs` — `BASELINE_SYNTHETIC_STUBS` 1263/1253, `SLACK = 0` | count drops by 2; gate goes RED |
| 5 | `scripts/baselines/jdk-only-gated-never-delete.tsv:86-87` | a **never-delete** gate names both triples |
| 6 | `scripts/baselines/jdk-only-kind-map-25-linux.tsv:5790-5791` | frozen kind map |

Plus `probes/DeadSweepReachProbe.java:330`, whose comment ("the receiver whose
`$1` the VM mints for the JavaSecurityAccess shared secret") is stale since
F17-1 but which does not dispatch to the triples.

**Correction to F17's stated blocker.** F17 named `stub_ratchet.rs:325` as a pin.
That line is prose inside a doc comment, not an assertion. The real ratchet
exposure is #4, the runtime count — and W7-62 records that this count is
**already firing** (1253 → 1261) for unrelated reasons, and that it *cannot be
recomputed by scanning source*. So the honest position is: this lane cannot
produce the new number, and hand-editing it would convert a live signal into a
permanent lie.

**Correction to the assignment itself, and this one matters most.**
`no_image_receiver.rs:143` is **not** a pin to be removed. That table records a
measured fact about the *image* — and the fact is TRUE: `javap -p
java.security.AccessController$1` → `class not found`. Its only function is to
re-tag registrations on that class `SyntheticStub` so `--jdk-only` refuses them.
Removing the line while the registrations exist would **promote two fabricated
natives to `Bridge` and admit them to strict mode** — the exact
`java.util.Properties` regression the module documents. The entry must stay until
after the registrations go, and then it is simply inert.

**Reachability, which was the one thing worth verifying before any deletion:**
nothing dispatches to either triple. The owner class is on no image, no bytecode
can name a method its own `SharedSecrets` does not declare, and since F17-1 no
factory hands out the owner (`owner_classes()` iterates `FACTORIES`). So
`call_native` has no reachable path to them and there is no panic risk either
way. The deletion is safe — it is just not this lane's to make.

---

## 4. NEW — a factory strict mode keeps, for an owner strict mode drops

This is a live `--jdk-only` capability defect, found while establishing §3.

`register_wp1_4_shared_secrets` sets **one** ambient kind, `Bridge`, for the
whole registrar (`shared_secrets_bridge.rs:2880`).
`NativeMethodRegistry::register` (`native-api/src/registry.rs:5820-5840`) then
re-tags **by receiver class**, via `receiver_declared_by_no_supported_image`.
Those two facts split the single registrar down the middle:

| registered on | in a table? | kind | `--jdk-only` |
|---|---|---|---|
| `jdk/internal/access/SharedSecrets` — the 14 factories | no; real JDK class | `Bridge` | **survives** |
| `cratonvm/internal/ss/JavaUtilJarAccess$1` and 3 siblings — their methods | yes, `VM_MINTED_STAND_IN_RECEIVERS` | `SyntheticStub` | **dropped** |

Note `jdk/internal/misc/SharedSecrets` **is** in `NO_IMAGE_JDK_RECEIVERS` and the
`jdk/internal/access/` spelling is not — correctly, only the legacy alias is
absent from the image. That is why the factory half escapes the re-tag.

So in `--jdk-only`, `SharedSecrets.javaUtilJarAccess()`,
`getJavaNetUriAccess()`, `getJavaNetHttpCookieAccess()` and
`getJavaIORandomAccessFileAccess()` are **still intercepted** — shadowing real
JDK bytecode that would have returned the real singleton — and hand back a
carrier on which every method has just been dropped.

### 4.1 PREDICTED symptom, and the discriminator

`make_factory_callback`'s `gen_factory!` calls `alloc_singleton(ctx, owner)`
(`shared_secrets_bridge.rs:283`), which is **infallible**:

```rust
match ctx.ensure_class_initialized(owner_class) {
    Ok(cid)  => { let n = ctx.class_num_total_fields(cid); ctx.alloc_object(cid, n.max(1)) }
    Err(_)   => ctx.alloc_object(cratonvm_types::ClassId::new(0), 1),
}
```

Two outcomes, both wrong, and which one occurs depends on a path outside these
files:

* if strict §5 refuses the fabrication → the `Err` arm returns an object of
  **`ClassId(0)`** typed as a `Java*Access`: a **wrong-class receiver**, not an
  error. `ensure_synthetic_class` was deleted 2026-08-10, which makes this arm
  reachable in a way it was not before.
* if `ensure_class_initialized` fabricates anyway (see `[Ok≠use]`) → right name,
  no methods, and the first interface call raises
  `AbstractMethodError`/`UnsatisfiedLinkError`.

`alloc_singleton`'s own comment asserts the good case — *"the invokeinterface
resolution still routes through the stored class name in the native registry"* —
and that premise is exactly what the strict re-tag invalidates: in `--jdk-only`
the registry no longer **has** those methods. A guard scoped by a stated premise
is only as good as the premise.

This is `no_image_receiver`'s own "half a fix" warning running in a **third**
direction: not re-tag-without-mint, nor mint-without-re-tag, but *re-tag an
owner's methods while leaving the factory that mints and returns that owner
untagged*.

### 4.2 What was landed for it

* `native-api/src/no_image_receiver.rs` — the mechanism documented on
  `VM_MINTED_STAND_IN_RECEIVERS`, with the rule stated for the next person:
  adding a `cratonvm/…` stand-in here is not complete on its own; ask what mints
  the receiver and what returns it to Java, and give that the same kind.
* `native-builtins/tests/stub_ratchet.rs` —
  `strict_keeps_no_shared_secrets_factory_whose_owner_it_dropped`, which freezes
  the orphan set at these four. It **does not assert the defect away**: a fifth
  orphan reddens it, and *fixing* one also reddens it, so the list can only
  shrink deliberately.

  Its premise is asserted, not assumed. `register_wp1_4_shared_secrets` reaches
  `register_boot_path` only **transitively**, through
  `register_essential_natives_with_shims` (`native-builtins/src/lib.rs:10063`) —
  a grep of the boot-path replay for `shared_secrets` finds **nothing**. This
  lane briefly concluded from that grep that the registrar was out of scope and
  the test would be vacuous; that was wrong, and the check now asserts
  `jdk/internal/access/SharedSecrets` is live so the day the indirection changes
  it fails loudly instead of passing on an empty set.

### 4.3 `java/io/ObjectInputStream$1` — nominated, not landed

It meets the table's rule on the one image measured (absent on 25.0.3; the JDK 25
source shows why), and `register_java_object_input_stream_access` registers on it
today as `Bridge`, surviving strict. But the table's threshold is **all six**
images and no JDK 21 image or source is on this host. A one-image add is the
under-measurement the module warns about, in the direction that demotes real
bridges. The rows are inert meanwhile — no factory hands out that owner and no
bytecode can produce a receiver of a class that does not exist — so the cost of
waiting is nil. Reasoned in place in the `NO_IMAGE_JDK_RECEIVERS` doc.

### 4.4 The 49-entry table, re-measured

All 49 `NO_IMAGE_JDK_RECEIVERS` entries were re-checked with `javap -p` on
25.0.3+9-LTS: **zero are declared**, so no entry demotes a real bridge on this
image. Run with positive controls (`sun.management.VMManagementImpl`,
`com.sun.jmx.mbeanserver.MXBeanMapping`, `jdk.internal.ref.CleanerFactory`,
`sun.nio.ch.FileDispatcherImpl`, `sun.reflect.annotation.AnnotationParser`, …)
so that "javap cannot see the module" was distinguishable from "the class is
absent" — without them a sweep of this shape reports its own reach. Two controls
came back missing and **both were errors in the control, not findings**:
`HotSpotDiagnosticMXBean` is `com.sun.*` not `sun.*`, and `java.rmi.activation`
was removed by JEP 407.

---

## 5. NOMINATIONS

Exact literal old/new. None applied — all are in files this lane does not own.

### N1 — `native-builtins/src/shared_secrets_bridge.rs`: the stale sync claim

Now false in the opposite direction: the test exists, and it is not "compile-time".

**old**
```
/// Mirrors `vm/src/runtime/shared_secrets.rs::SharedSecretsInterface`
/// (we can't `use` it here because native-builtins does not depend
/// on vm; a compile-time `#[test]` in the vm crate asserts the two
/// lists stay in sync).
```
**new**
```
/// Mirrors `vm/src/runtime/shared_secrets.rs::SharedSecretsInterface`
/// (we can't `use` it here because native-builtins does not depend
/// on vm). The mirror is NOT exact and must not be made exact: this
/// table has `java/io/FileDescriptor$1`, that one additionally has
/// `java/io/ObjectInputStream$1`, whose getter is deliberately not
/// intercepted. F24-1 (2026-08-13) added the check that keeps the
/// difference to exactly that — `owner_classes_mirror_the_native_builtins_bridge`,
/// a vm-crate unit test reading `owner_classes()` — after the claim
/// that used to sit here ("a compile-time `#[test]` in the vm crate
/// asserts the two lists stay in sync") turned out to describe a
/// test nobody had written, while the two lists disagreed in both
/// directions at equal length.
```

### N2 — `native-builtins/src/lib.rs:4527-4530`: stale header, names a deleted getter

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
(`getJavaSecurityAccess` was deleted by F17-1; JEP 486 removed the interface.)

### N3 — the `register_java_security_access` deletion (§3), as ONE commit

Not this lane's to make; the reachability question it was blocked on is answered
above (nothing dispatches to either triple). It requires, together:

1. `shared_secrets_bridge.rs` — delete `register_java_security_access`, its call
   at `:2894`, the bodies `jsec_do_intersection_privilege` /
   `jsec_get_protect_domains`, and the now-dead `gen_factory!(f_jsec, …)` at
   `:301` with its match arm at `:324`.
2. `shared_secrets_bridge.rs:3103` — drop the `java/security/AccessController$1`
   probe from `representative_method_registered_per_owner`, 15 → 14.
3. `scripts/baselines/jdk-only-gated-never-delete.tsv` — delete lines 86-87.
   (Their `registered_by` column already points at `shared_secrets_bridge.rs:2446`,
   which is stale — the function is at `:2649`. Regenerate, do not hand-edit.)
4. `scripts/baselines/jdk-only-kind-map-25-linux.tsv` — regenerate; do not
   hand-edit lines 5790-5791.
5. `native-builtins/tests/stub_ratchet.rs` — re-measure
   `BASELINE_SYNTHETIC_STUBS_*` and re-freeze **with the −2 attributed in
   prose**, per W7-62's rule. This requires running the VM. Note the gate is
   already firing (1253 → 1261) for unrelated reasons, so it must be settled
   first or the two changes will be indistinguishable.
6. `native-api/src/no_image_receiver.rs:143` — **leave the entry.** It becomes
   inert, and removing it is only correct once no registration on that class
   exists. Removing it *before* step 1 admits two fabricated natives to
   `--jdk-only`.
7. `probes/DeadSweepReachProbe.java:330` — correct the comment; the VM no longer
   mints that receiver.

### N4 — `java/io/ObjectInputStream$1` → `NO_IMAGE_JDK_RECEIVERS`

Blocked only on measurement (§4.3). `NO_IMAGE_JDK_RECEIVERS` is binary-searched,
so sorted position is load-bearing, not cosmetic — an out-of-order entry makes
the predicate answer `false` for a name that is in the list, silently. There is
no other `java/io/` entry, and `java/io/…` sorts before `java/lang/…`, so it
becomes the first `java/` row:

**old**
```
    "com/sun/net/httpserver/HttpExchange$ResponseBody",
    "java/lang/Compiler",
```
**new**
```
    "com/sun/net/httpserver/HttpExchange$ResponseBody",
    "java/io/ObjectInputStream$1",
    "java/lang/Compiler",
```

Apply only after a six-image run of `scripts/jdk-only-no-image-receivers.py`.
`table_is_sorted_and_unique` covers the ordering once applied.

---

## 6. Which mode each change reaches

| change | file | mode reached |
|---|---|---|
| spelling fixes, `JavaIOFileDescriptor` add | `vm/src/runtime/shared_secrets.rs` | **none at runtime** — the module has no consumer (§1.1). Documentation + test surface only |
| the three new/replaced guards | same, `#[cfg(test)]` | `cargo test -p cratonvm-vm`, both feature configs (no `cfg` on the path used) |
| `VM_MINTED_STAND_IN_RECEIVERS` doc | `native-api/src/no_image_receiver.rs` | documentation only; no table row changed, so no registration changes kind in any mode |
| `NO_IMAGE_JDK_RECEIVERS` doc | same | documentation only |
| `strict_keeps_no_shared_secrets_factory_whose_owner_it_dropped` | `native-builtins/tests/stub_ratchet.rs` | the ratchet's `CompatibilityMode::JdkOnly` census. Registrar reached transitively via `register_essential_natives_with_shims`, i.e. the **shipping** real-JDK path, not `register_synthetic_overrides` |

**No registration was added, deleted, or re-tagged by this lane.** No ratchet
count should move. If `BASELINE_SYNTHETIC_STUBS` moves after these patches,
something else did it.

## 7. Checks that FLIP vs. checks that merely become reachable

* **Flips red → green by the fix:** none. Nothing was red.
* **Would have flipped red on the corrected tree, so it was replaced:**
  `factory_method_matches_canonical_shape` (§2.3 control). This is the important
  one — repairing `javaUtilJarAccess` *requires* deleting that guard.
* **Newly able to fail at all** (was previously unwritable or vacuous):
  `owner_classes_mirror_the_native_builtins_bridge` (new capability — crosses the
  crate boundary), `owners_slice_equals_all` (contents, not length),
  `factory_method_matches_the_jdk25_surface` (external shape, both directions),
  `strict_keeps_no_shared_secrets_factory_whose_owner_it_dropped` (new).
* **Predicted-green, unverified:** all four. Not compiled, not run.

## 8. Left undone, and why

1. **Task 3's deletion** — spans six files, five unowned; two are generated
   baselines that must be regenerated, not hand-edited; one needs a VM run this
   lane may not do. Nominated in full as N3.
2. **`BASELINE_SYNTHETIC_STUBS` re-measure** — needs a build+run. Already firing
   per W7-62; not touched, on that record's own rule that a wrongly frozen gate
   is silent forever.
3. **The §4 defect itself is recorded, not fixed.** The fix is a kind decision
   (drop the four factories in strict, or tag them to match their owners) with a
   strict-corpus measurement attached, and it belongs with whoever owns
   `shared_secrets_bridge.rs`. The new test pins the blast radius meanwhile.
4. **`java/io/ObjectInputStream$1`** — one-image evidence only (N4).
5. **No type-check of any kind.** `rustfmt` parse only. In particular
   `owner_classes_mirror_the_native_builtins_bridge` introduces the **first**
   `cratonvm_native_builtins::` reference in `vm/src/runtime/` — the dependency
   exists in `vm/Cargo.toml` and is already used from `vm/src/jit/helpers.rs`,
   but that the path resolves from this module is **assumed, not verified**. It
   is the single most likely thing to fail on first build.
