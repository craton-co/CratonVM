# proxy bytecode generation, JPMS enforcement, JAR signature verification

Session slug: `proxy-and-modules`. Base: merged
`arch/wave1-integration-20260726` into the worktree branch before any analysis —
merge commit `18b2f49d5e1b90857acb3aa0fb5c905918f962d3`. `classloading/src/type_maps.rs`
is present in the merged tree.

Owned files: `classloading/src/{proxy_gen,module,jar_signer}.rs`. Everything else
cited is read-only; cross-file changes are collected in
[§5](#5-cross-owner-requests).

**Not built or run.** Nine agents share the host and builds are prohibited this
session. Changed files were parser- and format-checked with `rustfmt --check`
against copies (originals kept CRLF).

---

## 1. What `proxy_gen.rs` generates, and how it is verified, loaded and named

This is the deliverable the access-control wiring needs. Audited against the
merged tree.

### 1.1 They are ordinary classes, not hidden classes

`native_builtins::define_or_get_proxy_class`
(`native-builtins/src/reflect_annotations.rs:3373`) calls
`emit_proxy_classfile` and then `ctx.define_class_full(&gen_name, &bytes,
loader_id, opts)` with

```rust
DefineClassFull {
    skip_verification: false,          // :3470
    force_loader_faithful_linking: true, // :3486
    ..Default::default()
}
```

So: full Pass 2/3 verification runs, the trusted-hidden-class escape hatch is
**not** taken, `Class::hidden` is false, and the class therefore gets normal
type maps. The concern about generated classes silently skipping verification
(and hence precise GC oop maps) does not apply to proxies.

That `skip_verification: false` is only viable because every emitted body is
straight-line — no branch targets, no `exception_table` entries — so JVMS
§4.10.1 requires no `StackMapTable`. The emitter writes none. This is pinned by
`emitted_class_is_straight_line_no_handlers`, which scans every emitted `Code`
attribute for branch opcodes.

### 1.2 Name and package

`build_proxy_spec_for` (`reflect_annotations.rs:3598`) picks:

| condition | generated name |
|---|---|
| every proxied interface is `ACC_PUBLIC` | `jdk/proxy<M>/$Proxy<N>` |
| any interface is non-public, in package `p` | `p/$Proxy<N>` |
| any interface is non-public, default package | `$Proxy<N>` |

`<M>` is `proxy_module_number(loader_id)`, `<N>` a global counter. The
`jdk/proxy<M>` choice is deliberate: `java/*`, `sun/*` and `jdk/internal/*` are
rejected by `is_prohibited_package_name` for a non-bootstrap loader, and the
old hard-coded `java/lang/reflect/$ProxyN` always tripped that, silently
degrading every proxy to the shared `Proxy$Instance` shim.

### 1.3 Access flags

```
class:   ACC_PUBLIC | ACC_FINAL | ACC_SUPER | ACC_SYNTHETIC   (0x1031)
<init>:  ACC_PUBLIC                                            (1-arg and/or 2-arg)
<clinit>:ACC_STATIC
methods: ACC_PUBLIC | ACC_FINAL
m_<i>:   ACC_PRIVATE | ACC_STATIC | ACC_FINAL | ACC_SYNTHETIC  (0x101A)
```

**Deviation (open, needs a cross-file change — see §5.1).** The class flags are
unconditional. The JDK's `ProxyBuilder` emits `ACC_FINAL | ACC_SUPER` — i.e.
*package-private* — whenever any proxied interface is non-public. CratonVM
publishes a proxy of a package-private interface as a **public** class in that
interface's package. Two consequences:

* `Modifier.isPublic(proxyClass.getModifiers())` answers `true` where the JDK
  answers `false`. Spring's `ClassUtils` visibility helpers and Mockito's
  mock-visibility logic both read that.
* Once `check_class_access` is wired past `new` (it is currently live only
  there), the proxy remains reachable from any package, where HotSpot would
  confine it to the interface's package.

### 1.4 Nest

The class `attributes[]` table is emitted **empty**: no `NestHost`, no
`NestMembers`, no `InnerClasses`. `confirmed_nest_host`
(`access_control.rs:338`) maps `nest_host: None` to the class's own name, so a
`$ProxyN` is its own nest host and a nestmate of nothing else.

**That is the correct and safe answer here.** No emitted body touches another
class's `private` member. The only private members involved are the proxy's own
`m_<i>` static fields (same-class `GETSTATIC`/`PUTSTATIC`), and every
off-class target is public. Proxies do **not** have the lambda-body problem
where a runtime-generated class can never appear in a host's `NestMembers`
attribute.

### 1.5 Every cross-class member access an emitted body performs

This is the list a member-access wiring will start checking:

| site | target | accessibility |
|---|---|---|
| `<init>` | `INVOKESPECIAL <super>.<init>` | see below |
| method body | `INVOKESTATIC java/lang/reflect/Proxy$Dispatch.invokeProxy` | **see §1.6** |
| `<clinit>` | `INVOKEVIRTUAL java/lang/Class.getMethod` | public |
| `<clinit>` | `LDC class <iface>` | may be non-public, but the proxy is placed in the same package (§1.2) |
| `<clinit>` | `GETSTATIC <Wrapper>.TYPE` | public |
| method body | `INVOKESTATIC <Wrapper>.valueOf` | public |
| method body | `INVOKEVIRTUAL <Wrapper>.<prim>Value` | public |

The super is `java/lang/reflect/Proxy$Instance` by default — a synthetic stub
with `ACC_PUBLIC | ACC_SUPER` (`class_manager.rs:2024`
`synthetic_stub_access_flags`) whose `<init>` is declared `PUBLIC | NATIVE`
(`class_manager.rs:10536`). Cross-package `INVOKESPECIAL` on it is fine.

Under the `proxy_super_class_name()` gate the super becomes the *real*
`java.lang.reflect.Proxy`, whose constructor is `protected`. That call then
depends on the subclass clause of JVMS §5.4.4 **and** on
`receiver_ok_for_protected` not being satisfied vacuously by `receiver: None` —
the trap already flagged in `access_control.rs`'s STATUS block.

### 1.6 `Proxy$Dispatch` is a class that does not exist

Every generated method body ends in

```
INVOKESTATIC java/lang/reflect/Proxy$Dispatch.invokeProxy
    (Ljava/lang/Object;Ljava/lang/reflect/Method;[Ljava/lang/Object;)Ljava/lang/Object;
```

and `java/lang/reflect/Proxy$Dispatch` is **never defined as a class**. Its only
appearance outside `proxy_gen.rs` is a native-registry key at
`reflect_annotations.rs:2591`. Nothing calls `ensure_synthetic_class` for it
(only `Proxy$Instance` is ensured, at `reflect_annotations.rs:3433`), it has no
entry in `synthetic_stub_ctor_methods`, and `get_loaded_class_id` returns
`None` for it.

Today this is harmless because `check_module_access_by_id` fails open on
exactly that shape — `interpreter.rs:41800` guards the method-resolution check
with `if let Some(target_id) = cm.get_loaded_class_id(&class_name)`. A stricter
wiring that *loads* the methodref's owner class before checking, rather than
skipping when it is absent, turns **every dynamic-proxy call in the VM** into
`NoClassDefFoundError`. Since JDK proxies underpin Spring AOP, Hibernate and
annotation reflection, that presents as a whole-suite failure with no obvious
connection to access control.

If the wiring wants a real `Class` to check, `ensure_synthetic_class(
"java/lang/reflect/Proxy$Dispatch", 0)` plus a `PUBLIC | STATIC | NATIVE`
`invokeProxy` entry in `synthetic_stub_ctor_methods` is the shape that makes the
check pass honestly — see §5.2.

### 1.7 Module

`module_name` is assigned by `define_class_shared_with_options`
(`class_manager.rs:3639`) from `ModuleRegistry::module_for_package`. No module
declares `jdk/proxyN`, so the common case is `None` → unnamed module, and every
JPMS check short-circuits to allow. In the non-public-interface case the proxy
lands in the interface's package and inherits whatever module owns it; since app
classpath jars are registered `automatic = true` (§3.3) that too is
unconditionally permissive.

`Proxy$Instance` and every other synthetic stub get `module_name: None`
(`class_manager.rs:2252`), so the super link is module-invisible as well.

---

## 2. Bytecode correctness of the emitter

### 2.1 Defect: duplicate `interfaces[]` entries (JVMS §4.1) — FIXED

JVMS §4.1: *"No two entries in the `interfaces` array may be the same class or
interface."*

`build_proxy_spec_for` receives a `Vec<ClassId>` and
`define_or_get_proxy_class` deduplicates it **by `ClassId`**
(`reflect_annotations.rs:3401`). Two distinct `ClassId`s can share a name — that
is loader-based class identity, and this VM hits it routinely, which is why
`force_loader_faithful_linking` exists at all. So

```java
Proxy.newProxyInstance(l, new Class[]{ barFromLoaderA, barFromLoaderB }, h)
```

where both classes are named `com/foo/Bar` survives the ClassId dedup and
reaches the emitter as `interfaces: ["com/foo/Bar", "com/foo/Bar"]`. Because
`CpBuilder::add_class` deduplicates, the emitted `interfaces[]` held two
**byte-identical** u2 entries. HotSpot rejects such a classfile with
`ClassFormatError`; CratonVM's own reader accepts it, so the damage surfaces
later and elsewhere.

Fixed by collapsing repeats first-occurrence-wins before any CP allocation.
Order is preserved deliberately: `AopProxyUtils.proxiedUserInterfaces` trims
trailing infrastructure interfaces off the *end* of `getInterfaces()`, so
sorting or last-wins would break Spring.

Not raised as an error, because at the classfile level a repeated name is
indistinguishable from a single one. The *policy* — the JDK's
`Proxy.newProxyInstance` throws `IllegalArgumentException("repeated
interface")` — belongs to the caller, which alone can tell "same name, two
loaders" from "same class listed twice".

Tests: `duplicate_interface_names_are_collapsed_to_one_entry`,
`interface_dedup_preserves_first_occurrence_order`.

### 2.2 Defect: duplicate `methods[]` entries (JVMS §4.6) — FIXED

Same treatment for repeated `(name, descriptor)` pairs. `build_proxy_spec_for`
happens to deduplicate by that key today, but `ProxyClassSpec` is `pub` API and
the emitter is responsible for the structural invariant. The dedup runs *before*
`m_field_refs` / `m_field_meta` are allocated, because `emit_proxy_method`
indexes them positionally — `fields[]`, the `<clinit>` `PUTSTATIC` targets and
the per-method `GETSTATIC`s must all agree on one method list.
`emit_clinit`'s signature changed from `&ProxyClassSpec` to
`&[&ProxyMethod]` so the two lists cannot drift apart again.

Tests: `duplicate_method_signatures_are_collapsed_to_one_method`,
`field_count_matches_deduped_method_count`.

### 2.3 Audited and found correct

Recorded so it is not re-litigated:

* **`max_stack` / `max_locals`.** Both emitters use `4 + 2 * slot_count` with a
  floor of 4. Hand-traced peaks: proxy method 3 (0 params) / 6 (one 1-slot
  param) / 7 (one 2-slot param, from the `DUP; ICONST; DLOAD` window); `<clinit>`
  3 (0 params) / 6 (≥1 param). The formula meets or exceeds every case, exactly
  at 6 for the single-1-slot-param case. `max_locals = 1 + param_slot_count`
  (0 for `<clinit>`) is exact.
* **`checkcast` on the return type.** `L...;` is stripped to an internal name;
  array descriptors are kept verbatim, which is the correct `CONSTANT_Class`
  form for an array type.
* **Prior-art `equals` bug stays fixed.** The `Int`-family return path reads the
  actual descriptor return byte and casts to `Boolean`/`Byte`/`Character`/
  `Short`/`Integer` accordingly, rather than always `Integer` — the Spring
  `Binder`/`ObjectUtils.nullSafeEquals` CCE. The parameter side is symmetric
  (`int_family_param_wrapper` / `int_family_param_letter`), so a `Z` parameter
  boxes via `Boolean.valueOf(Z)`.
* **Default interface methods** get an emitted body like any other, so the
  `InvocationHandler` intercepts them instead of the interface's default
  implementation running directly. `is_default` is carried but unused in the
  body — correct.
* **Bridge methods** are collected by `build_proxy_spec_for` (they are public
  and non-static) and emitted; a bridge's erasure differs from the bridged
  method's, so both survive the `(name, descriptor)` dedup and both resolve
  under `Class.getMethod`.
* **Opcode encodings** (`aload`/`iload`/`lload`/`fload`/`dload` short forms,
  `iconst`/`bipush`/`sipush` selection, `ldc` vs `ldc_w`) are all correct.
* **Constant-pool overflow, oversized UTF-8, local-slot overflow and malformed
  descriptors** all surface as typed `ClassFileError` rather than panicking, and
  `CpBuilder::check` is called after all emission so a mid-emission failure
  cannot escape as a truncated classfile.

### 2.4 Known fidelity gaps, not fixed (no repro to justify the risk)

* **`<clinit>` has no exception handler.** The JDK's generated `<clinit>`
  wraps its `getMethod` calls in `try/catch`, converting
  `NoSuchMethodException` → `NoSuchMethodError` and `ClassNotFoundException` →
  `NoClassDefFoundError`. CratonVM's is straight-line, so a failed lookup
  propagates as `ExceptionInInitializerError` and the proxy class is
  permanently unusable. Adding handlers would require emitting a
  `StackMapTable` and would forfeit the §1.1 property, so this should only be
  done together with frame emission.
* **No `Exceptions` attribute** on emitted methods, so
  `proxyClass.getDeclaredMethod(...).getExceptionTypes()` is empty.
  `UndeclaredThrowableException` wrapping is unaffected — it reads the cached
  *interface* `Method`, not the proxy's.
* **Covariant returns are not collapsed.** The JDK's `ProxyGenerator` merges
  same-name/same-parameter methods into one with the most specific return type;
  CratonVM emits one shim per distinct descriptor. Both are valid classfiles;
  the `<clinit>` resolves the same `Method` into both slots.

---

## 3. `module.rs` — is JPMS enforced or modelled?

### 3.1 Verdict: enforced, but through a very narrow door

Enforced (error-producing) paths, all of which act on the result:

| check | production call site | outcome |
|---|---|---|
| `check_module_access` | `access_control.rs:470` (via `check_module_access_by_id`) | `LinkageError::IllegalAccessError` |
| `check_module_access_by_id` | `interpreter.rs:19742`, `:19798` (field resolution), `:41801` (method resolution) | `?`-propagated |
| `check_deep_reflection_access` | `vm_exec.rs:10426` → `lang_class.rs:673` and 9 reflection sites | `IllegalAccessException` / `InaccessibleObjectException` |

Modelled only (query returns a bool that nobody turns into an error):

* `is_package_exported_unqualified`, `is_package_exported_to`,
  `is_package_open_unqualified`, `is_package_open_to` — these back
  `Module.isExported` / `Module.isOpen` and nothing else.
* `reads` is enforced *indirectly* (it gates both `check_module_access` and
  `check_deep_reflection_access`) but its direct native, `Module.canRead`, is
  informational.
* `detect_cycles` has **zero** production call sites.

Dead wrappers: `check_class_access_with_modules`, `check_field_access_with_modules`
and `check_method_access_with_modules` (`access_control.rs:478/491/507`) have no
callers anywhere. Anyone adding member-level JPMS enforcement will reach for
these and should know they were never wired.

So `check_module_access_by_id` is indeed close to the only live path — three
call sites, all in `interpreter.rs`.

### 3.2 The registry is genuinely populated from real `module-info.class`

Not a model. `ClassManager::new` scans boot/ext/app class paths for
`module-info.class` and feeds each through
`descriptor_from_module_attribute` (`class_manager.rs:1820`), behind
`CRATONVM_BOOT_MODULE_REGISTRY` (default **on**). A second, lazy path registers
any `module-info` loaded as a class (`class_manager.rs:3609-3630`). A fallback
in `vm_init.rs:624-662` registers a hand-written `java.base` when both come up
empty.

### 3.3 …but app-classpath modules are registered `automatic = true`

`try_register_module_info` sets `automatic` for the application class path, and
`define_class_shared_with_options:3619` sets `desc.automatic =
!is_platform_module_name(&desc.name)`. `exports_package_to` / `opens_package_to`
short-circuit to `true` for automatic modules. Combined with the `is_empty()`
fail-open gate on every check, the practical posture is: **JPMS is enforced
between platform modules and effectively unenforced between application jars,
by design.** That is a defensible classpath-only model, but it means the
enforcement above rarely fires for user code.

### 3.4 Defect: `reads()` fallback dropped dynamic edges and honoured
`requires static` — FIXED

`reads()` has two modes: consult the cached closure when `graph_built`, else
fall back. The fallback was

```rust
self.modules
    .get(reader)
    .is_some_and(|desc| desc.requires.iter().any(|r| r.module_name == provider))
```

Two defects in that one expression.

**(a) `requires static` was honoured.** `build_readability_graph` step 1 filters
it (`if !req.is_static`) because a compile-time-only dependency is never
readable at runtime. So the same query answered `true` before the closure was
built and `false` after — a correctness answer depending on cache state.

**(b) Dynamic reads edges were invisible.** `add_reads` (the backing store for
`Module.addReads`, `--add-reads` and `NativeContext::module_add_reads`) patches
`readable` only `if self.graph_built`, parking the edge in `extra_reads`
otherwise. Nothing in the fallback consulted `extra_reads`.

Repro:

1. Boot with the default module registry → `build_readability_graph()` →
   `graph_built = true`.
2. `--add-reads A=B` or `Module.addReads` → `add_reads("A","B")` patches the
   cached closure. `reads("A","B") == true`. ✓
3. Any subsequent `module-info` class load → `register()` sets
   `graph_built = false` and `readable.clear()` (`module.rs:244-245`).
4. `reads("A","B")` now takes the fallback and returns **false**.

`check_module_access` and `check_deep_reflection_access` both gate on `reads`,
so inside that window the user's `--add-reads` turns into a spurious
`IllegalAccessError` / `InaccessibleObjectException`. The window closes only
when something calls `build_readability_graph()` again.

The fixed fallback mirrors both halves it is standing in for:
`build_readability_graph`'s seeding (non-static direct `requires`) plus
`add_reads`'s cached-graph patch (the edge itself and the `requires transitive`
closure it implies).

Tests: `dynamic_reads_survive_graph_invalidation_by_register`,
`dynamic_reads_imply_requires_transitive_before_rebuild`,
`requires_static_is_not_readable_with_or_without_the_closure`,
`non_static_direct_requires_still_readable_without_the_closure`.

### 3.5 Related, not owned

`vm_init.rs:677` rebuilds the readability graph only `if
!config.add_reads.is_empty()`. `--add-exports` / `--add-opens` alone do not
trigger a rebuild. Benign today (both are read straight out of
`extra_exports` / `extra_opens`), but the asymmetry is undocumented and will
bite if exports ever move into the closure.

---

## 4. `jar_signer.rs` — is signature checking real?

### 4.1 Verdict: real, and it fails closed

Not a stub. No `todo!()`, no unconditional `Ok(true)`, no env-var kill switch.

* **PKCS#7/CMS**: real ASN.1 walk of `ContentInfo → SignedData → SignerInfo`
  (`:337`). A `SignerInfo` with no authenticated attributes is **refused**
  rather than treated as trivially valid (`:423`).
* **`.SF` binding**: the `messageDigest` attribute is compared against a
  freshly computed digest of the `.SF` bytes; mismatch is a hard `Err` (`:478`).
* **Public-key signature**: really verified over the re-encoded `SET` of signed
  attributes per RFC 5652 §5.4. RSA PKCS#1 v1.5 is implemented in-tree
  (`:1501`, over a real `BigUint::modpow` at `:1258`, with `s >= n` and
  length checks and a constant-time EM comparison); ECDSA P-256/P-384 and DSA
  go through RustCrypto (`p256`, `p384`, `dsa` in `classloading/Cargo.toml`).
  Both `Bad` **and** `Unsupported` are treated as failure.
* **Manifest digests**: `.SF` ↔ `MANIFEST.MF` binding requires a real
  `<alg>-Digest-Manifest` match and explicitly refuses the weaker
  `-Digest-Manifest-Main-Attributes` substitute (`:3042`). Per-entry digest
  mismatch drops the whole signer block (`class_path.rs:2940`). A signed entry
  that is absent or unreadable is treated as tampering (`class_path.rs:2936`).
* **Chain**: `verify_chain` (`:2786`) cryptographically verifies every link,
  detects cycles, caps depth at 16, and returns `NoTrustAnchor` when no parent
  exists — self-signed leaves land there. RFC 5280 extension processing is real:
  unknown *critical* extension → reject; leaf KeyUsage must permit
  digitalSignature; leaf EKU, if present, must permit codeSigning; `cA=FALSE`
  rejected; `pathLenConstraint` enforced. Expiry is checked for leaf,
  intermediates and anchors.
* **Called from production**: `class_path.rs:2745` inside
  `extract_jar_signer_blocks`, reached from `find_class_code_source_info` →
  `class_manager.rs:4411` and `native-builtins/src/classloader.rs:6082`. Not
  dead code.
* **Not done, and documented as such**: CRL/OCSP revocation, NameConstraints
  and policy processing. One narrow deliberate fail-open: a cert validity
  timestamp that fails to parse skips *that bound only*, so a malformed date
  cannot cause a false reject.

### 4.2 The caveat that matters: verification is advisory, not a load gate

On **any** verification failure — bad signature, digest mismatch, no trust
anchor — the result is an empty certificate list and the class **still loads**
(`class_path.rs:2873` returns `Vec::new()` with a `debug!`;
`class_manager.rs:4422` builds `CodeSource::new(Some(url), certs)` with `certs`
empty). Nothing throws. HotSpot's `JarFile` throws `SecurityException` when a
signed JAR's entry digest does not match while reading the entry.

So the guarantee CratonVM provides is *certificate attribution*: a tampered
class in a signed JAR executes, it just loses its signer identity. If the threat
model assumes "signed JAR + tampered entry ⇒ refuse to run", that guarantee is
not present. Recorded as a finding, not changed — turning it into a hard failure
is a policy decision with real compatibility blast radius, and the change lands
in `class_path.rs`, which this session does not own.

Practical corollary: with no `JAVA_HOME/lib/security/cacerts`, no
`javax.net.ssl.trustStore` and no `CRATONVM_TRUST_PEM`, the trust store is empty
and **every** signed JAR reports as unsigned. Fail-closed in the right
direction, but silent below `warn!`.

### 4.3 Defect: a `pub` constructor that disabled verification — FIXED

`TrustStore::permissive_legacy_tests()` was a plain `pub fn`. The
`permissive_legacy` flag it sets disables *two* independent checks at once: the
SignerInfo public-key signature (`let enforce_pubkey = !trust_store.permissive_legacy`,
`:296`) and the entire chain walk (`:308`). A store built by it accepts any
signer block whose `.SF` digest is self-consistent — no signature math, no trust
anchor, no expiry.

Its only caller is `legacy_ts()` inside this file's `#[cfg(test)] mod tests`,
but as exported API it was a live fail-open switch reachable from every crate
depending on `cratonvm_classloading`, one call site away from silently turning
JAR signature verification off in a release build. Now `#[cfg(test)]`: a
non-test build has no way to construct a permissive store, and
`default()` / `empty()` / `load_default()` all leave the flag false. The flag
itself is untouched, so both branches still read it and there is no dead-code
churn.

(The other suspicious construct, the `OID_STUB_SIG` chain-link backdoor at
`:2456`, is already correctly `#[cfg(test)]`, with `is_stub_sig_fixture`
hard-`false` in non-test builds. Verified, left alone.)

Test: `production_trust_store_constructors_are_never_permissive` — also asserts
the test-only constructor really does flip the flag, so the other two assertions
are not vacuous.

---

## 5. Cross-owner requests

### 5.1 `native-builtins/src/reflect_annotations.rs` — proxy class access flags

To fix §1.3, `ProxyClassSpec` needs to know whether every proxied interface is
public. `build_proxy_spec_for` already computes exactly that predicate for the
package choice (`non_public_pkg`, `:3638`). Requested:

1. Add `pub all_interfaces_public: bool` to `ProxyClassSpec` (this session will
   land the field and the flag logic once the caller can be updated in the same
   commit — adding it now would break `native-builtins`, which this session does
   not own and cannot compile-check).
2. Set it to `non_public_pkg.is_none()` in the `ProxyClassSpec` literal at
   `:3748`.

The emitter side is then two lines: `ACC_PUBLIC` only when the flag is set.

### 5.2 `classloading/src/class_manager.rs` — `Proxy$Dispatch` has no class

See §1.6. Whoever wires JVMS §5.4.4 member access into `invoke*` must either
keep the "owner class not loaded ⇒ skip the check" behaviour that
`interpreter.rs:41800` already has, or make `java/lang/reflect/Proxy$Dispatch` a
real synthetic stub:

* `ensure_synthetic_class("java/lang/reflect/Proxy$Dispatch", 0)` alongside the
  existing `Proxy$Instance` call in `reflect_annotations.rs:3433`, and
* a `PUBLIC | STATIC | NATIVE` `invokeProxy` entry in
  `synthetic_stub_ctor_methods` (`class_manager.rs:10493`), mirroring the
  `Proxy$Instance.<init>` entry at `:10536`.

Without one of those two, every JDK dynamic proxy call in the VM fails.

### 5.3 `classloading/src/class_path.rs` — signature failure is not a load gate

See §4.2. If the intended posture is HotSpot's, `class_path.rs:2873` and the
per-entry mismatch at `:2940` need to produce a `SecurityException` rather than
an empty certificate list.
