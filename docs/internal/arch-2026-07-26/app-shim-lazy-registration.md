# App-shim lazy registration — feasibility and the seam that was landed

Session slug: `app-shim-lazy-registration`. 2026-07-26.
Base: `arch/wave1-integration-20260726` merged at `9d0b477852d6516cf2e152127c38c1febc23d3db`,
then `worktree-agent-a63934628b0ea41b9` (the native-builtins reachability audit)
merged at `dfc79f68afc34dbafd1af3b020093459194a5b46`.

Follows on from
[`native-builtins-reachability.md`](native-builtins-reachability.md) §4, "The
split that would actually pay": ~120 kLoC of third-party application shims that
the default build registers unconditionally for every program, described there
as "a clean seam \[with\] one call site each".

**Headline: lazy registration is not landable, and the seam is not clean.**
Both claims are load-bearing and both are backed by code below. What *was*
landed is the behaviour-preserving half — the shims are now grouped behind one
call each, with the obstruction to gating each family recorded as data and
enforced by tests.

---

## 1. Is late registration sound? **No — for exactly these natives.**

Two independent blockers. Neither is in a file this session owns.

### 1.1 The registry cannot be mutated after boot at all

`NativeMethodRegistry` is a plain struct with no interior mutability
(`native-api/src/registry.rs:3827`). It is held **by value**:

* `vm/src/vm/realms/native_realm.rs:15-16` — `/// Native method registry
  (immutable after construction).`
* built as a local at `vm/src/vm/vm_init.rs:1006`, every registrar runs against
  that local, and it is **moved** into the realm at `vm/src/vm/vm_init.rs:2542`,
  after which the whole `SharedVm` is `Arc`-wrapped.

`register()` needs `&mut Self` (`native-api/src/registry.rs:4060`). No `&mut`
exists past that move; the only `&mut shared.natives.native_methods` sites in
the tree are test/bench helpers that run before `Arc::new`. So late
registration is not merely unsound — it is unreachable without changing
ownership to a `RwLock` (and then ~60 `find` call sites take a read guard) or
adding a `Mutex`-guarded side table like JNI's
(`vm/src/native/jni.rs:4498`).

There is no freeze/seal flag and no init-phase assertion; the registry simply
stops being reachable.

### 1.2 The negative-memoization question — the good news, then the bad

This was the specific hazard to check, and the answer is not uniform.

**Sound, already fixed.** `NativeCallSite` (`native-api/src/native_id.rs:200`)
packs `(generation << 32) | (slot + 1)` into one `AtomicU64` and gates every
warm hit on `generation` (`native_id.rs:286`). A memoized *negative* is stored
as `encoded = 0` **keyed by generation** (`:387`), so it self-heals — pinned by
`memoized_negative_self_heals_when_a_native_is_registered_later`
(`native_id.rs:481`). `NativeMethodRegistry::generation()`
(`native-api/src/registry.rs:4728`) exists precisely for this and says so:
a memo "is valid exactly as long as the generation it was taken at still
holds". Its three consumers are all in `interpreter.rs` (`:30068`, `:39407`,
`:40272`) and there is a regression test that a late registration is seen
(`interpreter.rs:42991`).

**Sound.** An `ACC_NATIVE` method with no registered callback caches nothing —
`populate_invoke_cache` returns at `vm/src/runtime/interpreter.rs:32414`
without inserting, so every later call re-probes. `SharedVm::missing_natives_log`
is an append-only diagnostic, never consulted for dispatch. The slow-path
`invoke_or_native` (`vm/src/vm/vm_exec.rs:11389`) does ~16 live `find` calls
with no memo. JNI's table is a live probe.

**The `resolution.rs` warning overstates its case.**
`ResolvedMethod::native_target` (`classloading/src/resolution.rs:122`) carries a
19-line comment beginning *"MEMOIZED NEGATIVE — read before adding lazy native
registration"*. Its only reader, `vm/src/runtime/interpreter.rs:32203-32213`,
does `resolved.native_target.zip(..).or_else(|| ..native_methods.find_with_kind(..))`
— the `None` case falls straight through to a **live registry probe**. The
memoized negative is not load-bearing. (The residual risk on that field is the
opposite polarity: it stores a raw `NativeCallback`, and `generation()` only
moves on *new* triples — `registry.rs:4566-4583` — so a *re-registration* of an
existing triple is invisible there.)

**Unsound — and it is the case that matters here.**
`thread.invoke_cache`. When a method has real bytecode and no native,
`populate_invoke_cache` bakes `CachedInvokeTarget::Bytecode`
(`vm/src/runtime/interpreter.rs:32451`) into both `shared_resolution` and the
per-thread cache. The three dispatch arms — `interpreter.rs:32625`, `:39811`,
`:40656` — **never re-probe the registry**. The only escape is
`intercept_force_registered_native_cached`, which bails at `:30027` unless the
triple is on the hardcoded `force_native_over_real_jdk_bytecode` allow-list.
The only invalidations are class redefine and loader staleness; registering a
native bumps neither. `.clear()` reaches only `main_thread`
(`libcratonvm/src/lib.rs:653`, `vm-cli/src/main.rs:2822`).

`shared_resolution.promoted_invokes` (`vm/src/runtime/lockfree_resolve.rs:601`)
has the same defect but is incidentally cleared on every GC cycle
(`vm/src/memory/gc.rs:141`).

**Why that is fatal for *these* shims specifically.** Almost every registration
in the four families below overrides a method that *does* have real bytecode —
that is what an application shim is. So the one unsound cache is exactly the
one they would land in. A `HelloWorld` that never loads WildFly is fine either
way; a program that loads WildFly *after* first touching one of these call
sites would silently run the un-shimmed bytecode forever.

The JIT is clean: `jit/src/lib.rs` never references the registry, tier-up is
gated on a live `NativeCallSite::resolve` (`interpreter.rs:40272`), and no
deopt is needed because a registration simply makes the guarded path stop being
taken.

### 1.3 What *would* be sound: decide during `vm_init`, not after

Deciding at **registration time** has neither problem — nothing has executed,
so nothing has been memoized. And the inputs are already there:

* `ClassManager::new` runs at `vm/src/vm/vm_init.rs:612`, ~900 lines and three
  boot phases before `register_essential_natives` at `:1509`. Every `ClassPath`
  is constructed and every `entry_index` built by then.
* `entry_index: FxHashSet<String>` (`classloading/src/class_path.rs:338`, built
  by `build_archive_entry_index`, `:1348`) reads the zip central directory
  only — **no inflate**. A presence probe is one hash lookup per classpath
  entry.
* That is *not* the O(num_jars × zip-probes) rescan that made JAXB model
  building hang: that was `find_class_bytes_delegated` re-run per allocation,
  fixed by the sticky `synthetic_upgrade_absent` memo
  (`classloading/src/class_manager.rs:1319`). The lesson from it still applies —
  probe once, at boot, not per call.

The obstruction is plumbing only: `register_essential_natives(registry: &mut
NativeMethodRegistry)` (`native-builtins/src/lib.rs`) takes no VM handle, while
`class_manager` is a live local in the same function. Widening the signature is
a one-line change at `vm/src/vm/vm_init.rs:1509` — a file this session does not
own. There is no existing `has_entry`-style public probe; the closest is
`ClassManager::find_all_resource_urls` (`class_manager.rs:4389`), which is
index-only for plain JARs/dirs/jmods but *does* inflate on a hit for
`NestedJar` (`class_path.rs:3801`) and JImage (`:3877`).

**Specified, not landed:** a `pub fn has_entry(&self, name: &str) -> bool` on
`ClassPath` mirroring `find_all_resource_urls`' match arms but returning at the
first `entry_index.contains` hit and never building a URL, plus a
`ClassManager` wrapper. Everything it needs is already built eagerly.

---

## 2. The seam is not clean — two of four families are entangled with the JDK

The audit measured *reachability* (which registrars run). It did not measure
*which classes they register into*. Doing that changes the conclusion.

| Family | Registers into | JDK-module classes it **also** owns | Severable? |
|---|---|---|---|
| `AppIntrinsics` | `org/antlr/v4/`, `groovyjarjarantlr4/`, `net/bytebuddy/`, `org/mockito/`, `org/hibernate/` | — | **yes** |
| `BouncyCastle` | `org/bouncycastle/`, `org/springframework/security/crypto/bcrypt/` | — | **yes** |
| `DataSourcePools` | `io/agroal/`, `io/vertx/`, `org/infinispan/`, `org/jboss/jca/`, `org/jboss/as/connector/`, `com/arjuna/`, `javax/resource/`, `javax/transaction/` | `javax/sql/DataSource` (`java.sql`) | **no** |
| `JBossWildFlyXnio` | `org/jboss/`, `org/wildfly/`, `org/xnio/`, `io/undertow/`, `cratonvm/xnio/` | `java/security/AccessControlContext` (`java.base`), `javax/security/auth/Subject` (`java.base`), `javax/security/auth/login/LoginContext` (`java.base`), `javax/naming/InitialContext` (`java.naming`) | **no** |

Evidence: `wildfly_security.rs:1773` registers `AccessControlContext.<init>`;
`:1669` and `:1702` register the JAAS `Subject` / `LoginContext` surface;
`wildfly_naming.rs:1606` registers `InitialContext.<init>/lookup/getEnvironment`;
`wildfly_datasources_tx.rs` registers `javax/sql/DataSource`.

Gating either family on "is WildFly on the classpath?" would silently delete
JAAS and JNDI natives from every program that is not WildFly. That is the
`d8092acb` failure mode reached by a different route — and it is not
hypothetical: the H2 suite's `LoginContext` fix
(`h2-jaas-logincontext-and-properties-remove-fixed-20260722`) lives in
`wildfly_security.rs`, and H2 is not WildFly.

**Prerequisite for gating those two:** move the JAAS/JNDI/`DataSource`
registrations out of `wildfly_security` / `wildfly_naming` /
`wildfly_datasources_tx` into their own JDK-surface registrar. Then
`is_severable()` flips and the tests say so.

### Two audit classifications corrected

* **`xml_xerces` is not an application shim.** Every class it registers is
  `com/sun/org/apache/xerces/internal/**` (`lib.rs` `XERCES_*` constants) or
  `jdk/xml/internal/XMLLimitAnalyzer` — the JDK's own `java.xml` module,
  present in every JDK. The audit filed it under `native-appshim-data`; a
  classpath probe for it would be a no-op at best.
* **`jboss_jdkspecific` is not severable** either: it registers
  `java/lang/module/Configuration`, `java/lang/module/ResolvedModule` and
  `jdk/internal/module/SystemModuleFinders$SystemModuleReader`
  (`jboss_jdkspecific.rs:870`, `:919`, `:936`) — JDK module-system bridges that
  merely happen to be motivated by JBoss Modules. Left at its original call
  site.
* `servlet`'s Jython/ScriptEngine block registers
  `javax/script/ScriptEngineManager` (`servlet.rs:1214`, `java.scripting`)
  alongside the `org/python/**` shims, so it is not a pure family. Left alone.

---

## 3. What was landed

`native-builtins/src/app_shims.rs` (new). Four grouped registrars replacing 55
scattered calls plus one inline `registry.register`, taken from four
*contiguous* runs inside `register_essential_natives` so that **order is
preserved exactly**:

| Family | Was at `lib.rs` | Calls moved |
|---|---|---|
| `register_app_intrinsic_shims` | 7000–7007 (inside the existing `with_category(Intrinsic, ..)`) | 6 |
| `register_bouncycastle_shims` | 7593–7694 | 30 |
| `register_datasource_pool_shims` | 8735–8754 | 5 + both `real_*` guards |
| `register_jboss_wildfly_xnio_shims` | 16856–16936 | 10 + 1 inline registration |

Contiguity was the selection criterion, and it is what makes this safe:
`NativeKind` is *ambient* (`current_category` persists across `set_category`,
`registry.rs:3879`) and a later `register()` of the same triple overwrites an
earlier one, so **reordering** would be as dangerous as reclassifying. Nothing
was reordered, nothing was reclassified, nothing was dropped, and no new gate
or env var was introduced. Every family still registers on every boot,
including for `HelloWorld`.

`ShimFamily` carries the metadata a future conditional call site needs —
`witness_resources()` (the `entry_index` probe keys), `owned_prefixes()`, and
`jdk_entanglements()` — with `is_severable()` as the verdict.

### Tests (`app_shims.rs`, `#[cfg(test)]`)

* every family registers something;
* every registered class is under an owned prefix **or** a declared
  entanglement — so a registrar that grows a new out-of-family target fails
  loudly instead of silently widening what gating would remove;
* every declared entanglement is still actually registered — so the list cannot
  rot in the other direction and freeze a family as unseverable;
* `is_severable()` agrees with what the family measurably registers, and the
  two/two split is pinned explicitly;
* grouping leaks no ambient category, checked from all three `NativeKind`
  seeds;
* witnesses are `.class` resource names under the family's own packages;
* the JDK-bundled Xerces classifies as JDK and no family claims a JDK package.

The probe registry pins `set_drop_synthetic_stubs(false)`, because
`NativeMethodRegistry::new()` latches `CRATONVM_NO_STUBS` (`registry.rs:3979`)
and the JNDI entanglement inherits the registry's `SyntheticStub` default —
without the pin these tests would measure the shell rather than the registrars.

### Boot work removed from a `HelloWorld` run

**None, deliberately.** This change is behaviour-identical by construction; it
makes the removal a one-line edit rather than performing it. For sizing the
future change: the four families are ~110 kLoC of module source, and the
registry is ~3,100 entries at boot (`registry.rs:3956`), of which these
families are a low-thousands-of-`register()`-calls share. Each `register()` is
three `Box<str>` allocations plus two hash-map inserts — so the honest estimate
of the *time* saved is single-digit milliseconds against the 15–21 s a boot
already spends decompressing JDK jmods (`vm/src/vm/vm_init.rs:595-611`, ~28 000
entries / ~136 MB eagerly inflated). The payoff of this split is build time and
binary size, which needs the crate extraction — and `Cargo.toml` is not this
session's to edit.

---

## 4. Hot-path fixes (files owned by this session)

Continuing the predecessor's sweep. Both are per-call costs on paths that are
not obviously hot until you look at the caller.

**`net_uri_inet.rs::os_dns_nameservers_string()` forked a process.** On Windows
it runs `ipconfig /all` and parses the output. Its two callers are the natives
behind `sun/net/dns/ResolverConfigurationImpl.init0` and `.loadDNSconfig0`
(`lib.rs`), which sit under the real JDK's resolver-config refresh path — so a
long-running program re-paid a full process spawn every time the JDK decided
its config was stale. Now latched in a `OnceLock<String>`, matching
`resolve_real_hostname()`'s treatment from the previous session; the probe
itself moved to `os_dns_nameservers_string_uncached()` so the parsing stays
directly testable.

**`proxy_selector.rs::read_settings()` allocated 32 throwaway `String`s per
outbound connection.** It runs once per `ProxySelector.select(URI)`. The env
probe took a single key and derived both cases with `to_ascii_lowercase()` /
`to_ascii_uppercase()` — two allocations per probe naming a *constant* — and
then each call site wrote `env_get("http_proxy").or_else(|| env_get("HTTP_PROXY"))`,
even though `env_get` already tried both spellings, so the `or_else` arm re-ran
the identical pair of lookups. Net: 16 `getenv` calls (each taking the process
environ lock and scanning `environ` linearly) and 32 allocations, for 4 distinct
settings. Keys are now `&'static str` pairs and the duplicate arms are gone.
Semantics are unchanged — same keys, same lowercase-wins precedence — and five
new tests pin exactly that, since "we deleted a redundant lookup" is precisely
the kind of change that quietly drops a case.

Not fixed, deliberately: `util_time.rs::os_default_zone_id()` also reads `TZ`
per call, but it is only the fallback arm of `jvm_default_zone_id()`
(`util_time.rs:5165`) and is reached only when the real `TimeZone.getDefault()`
round-trip fails. Latching it would trade a cold-path `getenv` for a behaviour
change on `TZ`; not worth it.

## 5. Not built, not run

Concurrent builds OOM this host. Every edited file was parse-checked with
`rustfmt --edition 2021 --check` over a scratch copy of the whole
`native-builtins/src` tree (clean parse; `app_shims.rs`, `proxy_selector.rs`
and `net_uri_inet.rs` produce zero formatting diffs). The move was verified
mechanically: the set of `register_*(registry)` names deleted from `lib.rs` is
exactly the set added to `app_shims.rs`, and the one inline
`registry.register` (`ModuleXmlParser.parseModuleXml`) moved with it. CRLF line
endings preserved.
