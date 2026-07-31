# JDK-only mode — ambient native-category audit

| | |
|---|---|
| **Status** | Wave 1, measurement only. No behaviour was changed by this audit. |
| **Normative source** | [`feature-designs/jdk-only-mode.md`](feature-designs/jdk-only-mode.md) — the interface contract. This document does not define semantics. |
| **Companions** | [`jdk-only-audit.md`](jdk-only-audit.md) §2 (static inventory) · [`jdk-only-native-review.md`](jdk-only-native-review.md) (the promotion checklist this audit feeds) · [`known-issues/jdk-only/native-kind-is-ambient-and-defaults-to-syntheticstub.md`](known-issues/jdk-only/native-kind-is-ambient-and-defaults-to-syntheticstub.md) (the problem statement; this document is its measurement) |
| **Scope** | Every crate that calls `NativeMethodRegistry::register`. |
| **Evidence date** | 2026-07-31, against a JDK 25 image (`javap -p -s`) on Windows. |

`jdk-only-audit.md` §2 records that "the native kind is ambient, not per-call"
and calls a `register()` outside a `with_category(...)` block "a `SyntheticStub`
by omission, which a grep for `NativeKind::SyntheticStub` will never show."
This document is the follow-through: it maps where the ambient category actually
comes from at every registration site, says which sites are mis-tagged, and
specifies the mechanical procedure for converting those verdicts into category
changes once the schema-v2 census exists.

> **Citation convention.** File + symbol, not `file:line`, per
> `jdk-only-audit.md`. Nine agents are editing this checkout concurrently and
> line numbers drift within the hour. Where a line number appears below it is a
> reading taken at the evidence date and is marked as such.

---

## 1. How ambient classification works

`NativeMethodRegistry::register` (`native-api/src/registry.rs`) takes four
arguments and **none of them is a kind**:

```rust
pub fn register(
    &mut self,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    callback: NativeCallback,
)
```

The kind comes from a single mutable field on the registry,
`current_category`, which is initialised in the constructor to
`NativeKind::SyntheticStub` and is read — not passed — at the moment of
registration. Two APIs move it:

| API | Lifetime | Failure mode |
|---|---|---|
| `with_category(kind, f)` | lexically scoped to the closure; restores on return | The closure argument may be a **function pointer**, in which case the scope silently extends over that function's entire dynamic call tree. |
| `set_category(kind)` | persists until the next `set_category`, including across function returns | A `set_category` placed after a `register()` in the same function leaves that registration on the previous category. The idiom in this repo is a manual `let __prev = r.current_category(); … r.set_category(__prev);` save/restore, which is easy to get wrong and impossible to see with a grep. |

Three consequences follow, and all three are load-bearing:

1. **The category is a property of the caller, not of the registration.** A
   registrar function with no category call of its own inherits whatever the
   call chain left in the field. The same function called from two places can
   produce two different kinds for the same triples.
2. **A stub created by omission has no syntactic marker.** `rg
   'NativeKind::SyntheticStub'` cannot find it. Any census built on grep
   undercounts by construction — which is the premise this audit was opened to
   test.
3. **Registration is last-write-wins, so the category is decided by the LAST
   registration of a triple, not the first.** Two registrars can register the
   same `(class, name, descriptor)` under different categories, and the earlier
   tag disappears from the final registry without a trace. See §6.2 for a
   confirmed live instance.

### 1.1 The repository already documents two injuries from this

Neither is hypothetical; both are recorded in the code.

- **2026-07-14, `Function$Identity` and JMX.** `vm/src/vm/vm_init.rs` records
  that turning on `set_drop_synthetic_stubs(true)` unconditionally in real-JDK
  mode (dev `d8092acb`) broke WildFly boot the same day with
  `UnsatisfiedLinkError: Function$Identity.andThen` and an `ObjectName` NPE,
  and was reverted. Both clusters were permanent bridges wearing the
  `SyntheticStub` tag.
- **`Matcher.find` corruption.** A long comment inside
  `NativeMethodRegistry::register` describes an exception keyed on
  `NativeKind::Bridge` that accidentally matched a *legacy* synthetic-layout
  `Matcher.find` registration, because that registration had inherited `Bridge`
  "from a persistent `set_category(Bridge)` far above their registration site."
  The result was heap corruption of every real `Matcher`. The comment ends by
  instructing future readers not to change that registration's category
  "without re-auditing every `set_category`/`with_category` call between both
  `register_regex_natives` call sites and the top of
  `register_essential_natives`." That instruction is only necessary because the
  kind is ambient.

---

## 2. What this audit measured, and how

Two mechanical passes over `native-builtins`, `native-builtins-crypto`,
`native-builtins-security`, `native-collections`, `native-io`, `native-awt`,
`native-api` and `vm`:

1. **Lexical pass.** Comment- and string-stripped source, brace-matched scopes,
   a per-function state machine tracking `with_category` push/pop,
   `set_category`, and `let x = r.current_category()` capture/restore. Each
   `.register(` call is attributed to the category in effect at that point.
2. **Call-graph pass.** For every registration with no lexical category, the
   callers of its enclosing function are resolved recursively until a
   categorised ancestor is found, producing the set of categories that
   registration can be created under.

Verdicts were then checked against the real class library with
`javap -p -s` on JDK 25: for each statically resolvable `(class, name,
descriptor)`, whether the class exists in the image, whether the method exists,
and whether it is `ACC_NATIVE`, abstract, or has concrete bytecode. This is
exactly questions 1–3 of [`jdk-only-native-review.md`](jdk-only-native-review.md),
run in bulk.

**Limits of this method, stated up front.** It is a *source* census, not a
runtime one. It cannot see which registration wins an overwrite, it counts
`#[cfg]`-gated code that the default build never compiles, and roughly a fifth
of registrations name their class through a loop or `let` variable this pass
could not resolve. Every number below is a static reading. Nothing here
substitutes for `invocations`.

---

## 3. Per-crate map of registration entry points

Registration **call sites** in source, by the category in effect at the site.
"Ambient" means no lexical category — resolved through the call graph in the
last column.

| Crate | `.register(` sites | Bridge | Intrinsic | SyntheticStub | Parameterised | Ambient | Ambient resolves to |
|---|---:|---:|---:|---:|---:|---:|---|
| `native-builtins` | 11,709 | 6,350 | 1,695 | 433 | 22 | 3,209 | mostly `Bridge` via `register_essential_natives_with_shims`; 83 to `SyntheticStub`; see §5 |
| `native-collections` | 1,219 | 1,195 | 0 | 15 | 7 | 2 | `Bridge`, from `register_collections_natives` |
| `native-io` | 1,129 | 1,105 | 0 | 24 | 0 | 0 | — |
| `native-awt` | 122 | 0 | 0 | 0 | 0 | 122 | `Bridge`, from one function-pointer `with_category` |
| `native-builtins-security` | 3 | 0 | 0 | 0 | 0 | 3 | `Intrinsic`, from the caller in `native-builtins` |
| `native-builtins-crypto` | 0 | — | — | — | — | — | crate performs no registration at all |
| `native-api` | 84 | 10 | 5 | 9 | 1 | 59 | almost entirely test fixtures |
| `vm` | 562 | 13 | 0 | 1 | 0 | 548 | mostly test fixtures; see §5.2 for the two that are not |

### 3.1 The ambient category at the boot sequence is `SyntheticStub`

This is the fact that makes the rest of the table readable. `vm/src/vm/vm_init.rs`
constructs a fresh `NativeMethodRegistry`, whose `current_category` starts at
`SyntheticStub`, and then makes roughly a hundred `register_*` calls. Only five
short windows in that function raise the ambient category to `Bridge` (each a
`let __prev = current_category(); set_category(Bridge); … set_category(__prev);`
pair). **Every registrar called outside those five windows runs at the
`SyntheticStub` default**, and is saved only by setting its own category.

Most do. `register_io_natives`, `register_collections_natives`,
`register_awt_natives`, `register_essential_natives_with_shims`,
`register_properties_sidetable`, the `lang_invoke` family, the `jmx` family and
the `phases_late` family all set their own category on entry and restore it on
exit. §5 lists the ones that do not.

### 3.2 `native-builtins-crypto` — no exposure

Zero `register` calls and zero category calls. The crate exports pure kernels
(`bc_aes`, `bc_chacha`, `bc_newhope`, `signature`) that `native-builtins`
marshals and registers. Classification for anything backed by these kernels is
decided at the `native-builtins` call site. Annotated in
`native-builtins-crypto/src/lib.rs`.

### 3.3 `native-builtins-security` — correct, but only because of one caller

`register_sunec_intpoly_intrinsics` (2 registrations) and
`register_sunec_point_intrinsics` (1 registration) carry no category. Both are
wrapped by `registry.with_category(NativeKind::Intrinsic, …)` at their single
call site in `native-builtins/src/lib.rs`. That is the correct kind — these are
Montgomery field arithmetic and EC scalar multiplication over methods that have
concrete bytecode — but the correctness lives in the caller. Deleting or moving
that wrapper turns all three into synthetic stubs, and they would then be
refused under `--jdk-only` with no compile error and no grep-visible marker.
Both are annotated `unknown — needs census`: the intrinsic *claim* also still
owes the measurement that `jdk-only-native-review.md` §6 requires, and
`sunec_point`'s `gate_enabled()` guard means the registration may not happen at
all, which a source census cannot see.

### 3.4 `native-awt` — 122 registrations, one judgement

The whole crate is tagged by a single line:

```rust
pub fn register_awt_natives(registry: &mut NativeMethodRegistry) {
    registry.with_category(NativeKind::Bridge, natives::register_all);
}
```

`natives::register_all` is a **function pointer**, so the `Bridge` scope covers
`register_all`'s entire dynamic call tree — all ten `register_*_natives` groups
and all 122 registrations — none of which contains a category call of its own.

The evidence says that tag is wrong for most of them. Against JDK 25:

| Group | ACC_NATIVE | has bytecode | abstract | absent | verdict |
|---|---:|---:|---:|---:|---|
| `register_toolkit_natives` | 2 | 2 | 4 | 1 | unknown (mixed; split) |
| `register_headless_natives` | 1 | 5 | 0 | 0 | bridge for `hasDisplays0`, stub for the rest |
| `register_component_natives` | 0 | 11 | 0 | 0 | stub |
| `register_frame_natives` | 0 | 4 | 0 | 2 | stub |
| `register_graphics_natives` | 0 | (27, class from loop var) | — | — | stub |
| `register_image_natives` | 7 | 15 | 0 | 1 | unknown (mixed; split) |
| `register_event_natives` | 0 | 7 | 0 | 0 | stub |
| `register_font_natives` | 0 | 12 | 0 | 0 | stub |
| `register_swing_natives` | 0 | 15 | 0 | 1 | stub |
| `register_clipboard_natives` | 0 | 3 | 0 | 0 | unknown (OS resource, wrong layer) |

`java.awt.Component`, `java.awt.Frame` and `java.awt.Toolkit` each declare
exactly one `ACC_NATIVE` method in JDK 25 — `initIDs()V` — and
`java.awt.Graphics`, `java.awt.GraphicsEnvironment` and all of `javax.swing`
declare none. The genuine bridges here are the `initIDs` no-ops, the
`PlatformGraphicsInfo.hasDisplays0` display probe, and the seven
`com.sun.imageio.plugins.jpeg.JPEG*` entries that back libjpeg. The other ~105
are re-implementations of class-library behaviour over this crate's Rust peer
model.

### 3.5 `native-collections` — 1,195 registrations, zero bridges

`register_collections_natives` opens with `set_category(NativeKind::Bridge)` and
restores on exit. Every one of its ~45 callees inherits that. Result: 1,195 of
the crate's 1,219 registrations are `Bridge`.

**Not one of them targets an `ACC_NATIVE` method.** Of the resolvable triples:
622 shadow methods with concrete bytecode, 214 land on abstract interface
methods, 91 name methods absent from JDK 25, 30 name absent classes, 6 have a
descriptor that does not match the real one. `java.util` is pure Java; there is
no VM or OS boundary in this crate to bridge to.

The 214 abstract-interface registrations deserve separate emphasis: a native on
an abstract `java.util` interface method intercepts **every** implementing
class, including application-defined ones. The tag chosen there governs
dispatch far outside `java.util`.

The crate's only per-subsystem judgements are the `SyntheticStub` block in
`register_linked_blocking_deque_stub_natives` (correct — `LinkedBlockingDeque`
declares no natives, and the code itself calls the implementation "a
correctness-only stopgap") and the `kind` parameter of
`register_string_joiner_natives_with_category`.

That last one is the cleanest worked example in the repository of why the kind
should not be ambient: the **same seven registrations of the same triples with
the same callbacks** are emitted under `Bridge` by
`register_string_joiner_natives` and under `SyntheticStub` by
`register_string_joiner_stub_natives`. Nothing distinguishes them except the
argument the helper was handed — and that argument decides whether they survive
`--jdk-only`.

### 3.6 `native-io` — the tag is often right, but by placement

`register_io_natives` sets `Bridge` on entry; the per-module registrars it calls
(`process`, `net`, `nio_native`, `file_channel`, `random_access_file`, …) each
set `Bridge` explicitly as well. Zero registrations in this crate are lexically
uncategorised.

Unlike `native-collections`, this crate *does* own real boundaries: 86 of the
resolvable `Bridge` triples are `ACC_NATIVE` in JDK 25. But 307 shadow concrete
bytecode, 105 are abstract, 91 name absent methods and 54 name absent classes.

| Entry point | ACC_NATIVE | bytecode | abstract | absent | verdict |
|---|---:|---:|---:|---:|---|
| `net::register_sun_nio_ch_net` | 36 | 3 | 0 | 20 | **bridge** |
| `process::register_process_natives` | 8 | 1 | 0 | 1 | **bridge** |
| `random_access_file::register_random_access_file_natives` | 8 | 0 | 0 | 1 | **bridge** |
| `nio_native::register_nio_natives_real` | 5 | 2 | 0 | 5 | **bridge** |
| `lib::register_io_natives` (direct) | 25 | 78 | 2 | 8 | mixed |
| `lib::register_scanner_natives` | 0 | 35 | 2 | 0 | **stub** |
| `lib::register_data_stream_natives` | 0 | 25 | 4 | 4 | **stub** |
| `lib::register_string_rw_natives` | 0 | 22 | 1 | 0 | **stub** |
| `stream_decoder::register_stream_decoder_natives` | 0 | 9 | 0 | 0 | **stub** |
| `stream_encoder::register_stream_encoder_natives` | 0 | 11 | 0 | 0 | **stub** |
| `nio_selector::register_nio_selector_real` | 3 | 2 | 0 | 37 | unknown (dead-registration candidate) |
| `async_socket::register_async_socket_real` | 0 | 10 | 16 | 9 | unknown (registered a layer too high) |
| `pipe::register_pipe_real` | 0 | 3 | 2 | 12 | unknown (registered a layer too high) |
| `direct_buffer::register_direct_buffer_real` | 0 | 9 | 1 | 11 | unknown (real boundary is `Unsafe`) |
| `datagram::register_datagram_real` | 0 | 0 | 4 | 1 | unknown |
| `nio_native::register_t16_channel_overrides` | 0 | 6 | 9 | 8 | unknown |

A recurring pattern in the "unknown" rows: the behaviour genuinely is a
boundary (a pipe, a socket, a direct memory region) but the registration sits on
the **abstract public API** — `java.nio.channels.Pipe`,
`AsynchronousSocketChannel`, `DatagramChannel` — rather than on the
`sun.nio.ch.*Impl` class where the JDK actually declares its natives. Those are
neither bridges nor stubs as written; they are bridges in the wrong place, and
moving them is a behaviour change that needs the census first.

---

## 4. The two named cases: JMX and `Function.identity()`

Both were chased specifically. **Both are already fixed, and neither is in the
residual 157.**

### 4.1 `Function$Identity` — fixed, but now over-tagged

`native-builtins/src/lib.rs`, `register_function_identity_natives`, sets
`NativeKind::Bridge` explicitly and carries a comment naming the 2026-07-14
regression: tagging it `SyntheticStub` broke real-JDK boot with
`UnsatisfiedLinkError: Function$Identity.andThen` the moment
`set_drop_synthetic_stubs(true)` began actually dropping registrations. Five
registrations: `Function.identity`, `UnaryOperator.identity`, and
`Function$Identity`'s `apply` / `andThen` / `compose`.

The retag stopped the bleeding and is the right call for today. It is **not**
the right end state, and the next wave should not read `Bridge` here as
settled:

- Real OpenJDK's `Function.identity()` returns `t -> t`, an `invokedynamic`
  lambda. There is no classfile named `Function$Identity` in any JDK image, so
  by `jdk-only-native-review.md` rule 1 this is a **CompatibilityShim** — the
  disposition table's "stands in for a runtime-generated artifact → move to the
  generated-class service" row, verbatim.
- Under the wave-1 contract these two halves now disagree. Contract §4 lets the
  `Bridge` native register under `JdkOnly`; contract §5 requires
  `ClassManager` to refuse to fabricate `java/util/function/Function$Identity`
  and record a `CompatibilityClassRequested` violation. The native survives
  against a class that cannot exist. That is a cross-check the strict-mode
  smoke test should assert explicitly.
- `classloading/src/class_manager.rs` still fabricates the class through
  `ensure_synthetic_class` and still carries a `synthetic_stub_fields` entry for
  it, and `native-builtins` allocates instances via
  `alloc_concurrent_synthetic(ctx, "java/util/function/Function$Identity", 0)`
  in two places. The real fix is a `GeneratedLambda`-origin implementation, not
  a better tag.

### 4.2 JMX — fixed, and the fix is broad

`native-builtins/src/jmx.rs` contains 230 registrations across ~16 registrar
functions. 203 are covered by an explicit `set_category(NativeKind::Bridge)` /
restore pair; one of those functions carries the note `UPDATED 2026-07-14: this
was previously tagged SyntheticStub`. Two are deliberately and correctly kept
as stubs (`register_mbean_server_factory_synthetic`).

The remaining **25 are still ambient**, and they are the ones that broke:
`register_object_name` (23 registrations) and `register_object_instance` (2)
set no category and inherit `Bridge` from `register_jmx_natives`, which opens
its own `set_category(Bridge)` window and calls them immediately. The tag is
right today. It is right for exactly the same structural reason it was wrong on
2026-07-14 — a decision made in an ancestor frame — and `javax.management.
ObjectName` is the class the reverted change NPE'd on. Pin these two functions
first when `register_as` lands (§8.3, stage 2); it is a no-op on behaviour and
it removes the only remaining ambient dependency in the JMX surface.

`vm/src/vm/vm_init.rs` records the same history from the other side, in the
comment explaining why `set_drop_synthetic_stubs(true)` is **not** called in
either real-JDK arm: "several SyntheticStub-tagged register_* clusters (JMX,
Function$Identity) are permanent bridges needed in real mode too."

### 4.3 What this means for the report's premise

The report's central claim — that ~157 residual stubs include permanent bridges
mis-tagged by ambient omission — was **true when it was written and is now
largely spent**. The two named clusters were retagged in the same change that
reverted the 2026-07-14 drop. The dominant remaining ambient-classification
defect in this repository runs in the **opposite direction**: wholesale
`Bridge` tags applied to entire crates, covering ~1,300 registrations across
`native-collections` and `native-awt` that target no native method at all.

That inversion matters for wave-2 planning. Over-tagging as `Bridge` does not
break a boot; it silently inflates the bridge count, hides real stubs from the
census, and lets a compatibility shim survive `--jdk-only` — a correctness leak,
not a crash. It is safe to work on incrementally, which under-tagging never was.

---

## 5. Sites that are `SyntheticStub` purely by omission

These are registrars that set no category of their own and are reached from
`vm_init` outside the five `Bridge` windows. Ordered by how bridge-shaped the
behaviour is.

### 5.1 Live candidates for mis-tagged permanent bridges

**`native-builtins/src/util_concurrent_ext.rs :: register_concurrent_natives`
— 53 registrations, and the strongest candidate in this list.**
Called from `vm_init` in **both** real-JDK arms at the default ambient
category, so all 53 are `SyntheticStub`. The `vm_init` comment immediately
above the call reads: *"Real-JDK apps still need ReentrantLock / Condition / LBQ
drainTo natives (SLF4J replayEvents, Spring thread pools)."* A registration the
boot sequence describes as needed in real mode, tagged with the one kind
`--jdk-only` refuses, is precisely the JMX / `Function$Identity` shape — still
live. Complication: much of the function is behind `if !real_aqs`, which is
false by default, so the *registered* subset is smaller than the source count
and only `invocations` can say which entries actually exist. Verdict:
**unknown, high priority**.

**`native-builtins/src/util_concurrent_ext.rs :: register_stamped_lock_natives`
— 31 registrations, and a confirmed silent downgrade.**
Registered twice with two different kinds: once from
`register_essential_natives_with_shims` under `Bridge`, and again from
`vm_init` at the default ambient, which runs **later**. Last-write-wins means
the surviving entries are `SyntheticStub` and the `Bridge` tag never appears in
any dump. This is the exact defect `NativeCensusEntry::overwrote` was added to
surface. On the merits `java.util.concurrent.locks.StampedLock` declares no
`ACC_NATIVE` method in JDK 25, so `SyntheticStub` is defensible as an outcome —
but nobody chose it, and the code that did choose `Bridge` is still there
believing it won.

**`vm/src/runtime/instrument.rs :: register_instrumentation_natives` (48) and
`register_self_attach_natives` (4).**
`java.lang.instrument` is class definition and redefinition — a VM service, on
`jdk-only-native-review.md` §5's bridge list without qualification. These are
registered from `vm_init` **twice, at different categories in different `#[cfg]`
arms**: `Bridge` inside an explicit window in the
`#[cfg(not(feature = "synthetic-jdk"))]` arm, and at the bare default in the
`synthetic-jdk` arm. The default build is fine; the `synthetic-jdk` feature
build tags 52 class-redefinition bridges as synthetic stubs. The asymmetry is
almost certainly an oversight, since the explicit `Bridge` window exists in only
one of two otherwise-parallel arms. Verdict: **bridge**, in both arms. Not
annotated here — `vm/**` is out of scope for this audit.

**`native-builtins/src/lib.rs :: register_forkjoin_quiescence` — 1
registration.** Reached only from `vm_init`, at the default ambient, in both
arms, so it is unconditionally `SyntheticStub`. The `vm_init` comment says it
must run after `register_concurrent_natives` because "the real one polls this
crate's async worker pool, which `native-collections` cannot see" — i.e. it
bridges to VM-owned thread state. `ForkJoinPool.awaitQuiescence` has concrete
bytecode in JDK 25, so this is not a clean bridge either. Verdict: **unknown**.

### 5.2 Omissions that happen to land on the right tag

Recorded so the next wave does not spend time on them, and to show that
"ambient" is not a synonym for "wrong".

| Site | Registrations | Why the tag is right anyway |
|---|---:|---|
| `native-builtins/src/lib.rs`, first registration in `register_essential_natives_with_shims` | 1 | `MergedAnnotation$Adapt.isIn` is registered *before* the function's own `set_category(Bridge)` line, so it inherits the boot default. It genuinely is a Spring compatibility shim, so `SyntheticStub` is correct — by luck of line ordering. Move that `set_category` up three lines and the tag flips. |
| `native-builtins/src/classfile_api.rs`, 11 registrars, 83 registrations | 83 | Resolved through the call graph to `SyntheticStub` set by the parent `register_classfile_api_natives`, with a comment explaining that every JEP 484 Class-File API native returns a placeholder. Deliberate and documented, not an omission. |
| `native-builtins/src/logging_shims.rs :: register_slf4j_binder_stubs_pub` | 33 | Third-party SLF4J binder shims; no such class in any JDK image, so rule 1 fires. |
| `native-builtins/src/deprecated_io_util.rs :: register_url_codec` | 6 | `URLEncoder`/`URLDecoder` have concrete bytecode. |
| `native-builtins/src/phases_late/zip_streams.rs :: register_p59_zip_output_primitives`, `register_p59_bulk_stream_transfer` | 10 | `ZipOutputStream.writeShort`/`writeInt`/`writeLong` are private helpers with bytecode. |
| `native-builtins/src/service_loader.rs :: register_service_loader_natives` | 11 | Reached both from `jdbc.rs` under `Bridge` and from `vm_init` at the default. Ordering decides; `ServiceLoader` has no natives, so `SyntheticStub` is the right outcome either way. |

---

## 6. Verdict counts

41 `// JDK-ONLY-CLASSIFY:` annotations were added, all comment-only. No
registration changed category and no code changed behaviour.

| Crate | bridge | stub | unknown | n/a | Registrations covered |
|---|---:|---:|---:|---:|---:|
| `native-io` | 4 | 6 | 8 | 0 | ~1,129 (all entry points) |
| `native-awt` | 1 | 8 | 4 | 0 | 122 (all) |
| `native-collections` | 0 | 4 | 3 | 0 | ~1,219 (crate entry + 6 groups) |
| `native-builtins-security` | 0 | 0 | 2 | 0 | 3 (all) |
| `native-builtins-crypto` | 0 | 0 | 0 | 1 | 0 |
| **Total** | **5** | **18** | **17** | **1** | |

`unknown` outnumbers `bridge` deliberately. A wrong `bridge` verdict is how the
2026-07-14 regression gets reintroduced from the other side — a shim promoted
past the strict-mode filter — so every case that needed a runtime fact to
settle was left as `unknown` with the missing evidence named in the comment.

`native-builtins/**` was read but not annotated: the contract reserves the
157-stub reclassification for a later wave with runtime evidence and
subsystem-per-PR discipline. §4 and §5 above are that crate's findings.

### 6.1 Grepping the annotations

```bash
rg -n 'JDK-ONLY-CLASSIFY' native-io native-collections native-awt \
   native-builtins-crypto native-builtins-security
rg -n 'JDK-ONLY-CLASSIFY: bridge'  # 5
rg -n 'JDK-ONLY-CLASSIFY: stub'    # 18
rg -n 'JDK-ONLY-CLASSIFY: unknown' # 17
```

### 6.2 The overwrite hazard, with a reproducible instance

`native-io/src/lib.rs :: register_io_natives` registers
`java/io/FileInputStream.read([BII)I` **twice inside the same function**: once
in the `SyntheticStub`-tagged public-surface block, and again about 50 lines
later under the ambient `Bridge`. The second registration wins. The final
registry shows one `Bridge` entry; the `SyntheticStub` tag that the author
wrote, and the `vm_exec` bytecode-preference behaviour it was meant to buy,
are gone without a trace in any dump.

`read([BII)I` is *not* `ACC_NATIVE` in JDK 25 — only the private `readBytes`
is — so `Bridge` is wrong on the merits. It is annotated `unknown` rather than
`stub` because removing the later line also changes which callback wins, which
is a behaviour change requiring the census.

This one site is the argument for `NativeCensusEntry::overwrote` in a sentence:
without it, the difference between "tagged `Bridge`" and "tagged `SyntheticStub`
and then silently overwritten by a default-category `register()`" is
unobservable.

---

## 7. Mechanical procedure for wave 2

Do not act on any verdict in this document from the source evidence alone.
The procedure below turns a verdict into a category change using the schema-v2
census fields from contract §4 (`registered_by`, `overwrote`, `invocations`).

**Precondition.** The schema-v2 fields must actually be in the tree. At the
evidence date `native-api/src/registry.rs` contains no `census()`,
`NativeCensusEntry`, `record_invocation`, `invocations_of_kind` or
`allowed_in`; a working copy that had them was replaced by a merge during this
audit. Confirm before starting:

```bash
rg -n 'fn census|NativeCensusEntry|record_invocation|allowed_in' native-api/src/registry.rs
```

### Step 1 — capture a census that covers the subsystem

```bash
export JAVA_HOME=/path/to/jdk25
cargo run -p cratonvm-cli --bin cratonvm -- --real-jdk --java-home "$JAVA_HOME" \
  --dump-native-registry target/jdk-only-audit/registry-real.json \
  -cp <corpus that exercises the subsystem> <Main>
```

`HelloWorld` is not a corpus. A `native-awt` verdict needs a run that draws;
a `native-io` selector verdict needs a run that selects. Per
`jdk-only-native-review.md` §7, take the census over the regression suite and
the differential corpus, not one program.

### Step 2 — resolve the three census fields per triple

```bash
jq -r '.natives[]
  | select(.class == "<CLASS>" and .name == "<NAME>")
  | [.kind, (.overwrote // "-"), (.invocations // 0), (.registered_by // "-"),
     .real_declaring_method.present, .real_declaring_method.acc_native,
     .real_declaring_method.has_code]
  | @tsv' target/jdk-only-audit/registry-real.json
```

Read them in this order, and stop at the first rule that fires:

| Reading | Action |
|---|---|
| `overwrote` is non-null and differs from `kind` | **Fix the overwrite first.** The tag you are looking at is not the tag anyone wrote. Resolve which registration should win before classifying. §6.2 is the worked example. |
| `invocations == 0` across the whole corpus and no test names the triple | Dead registration. Delete outright and lower the ratchet baseline in the same change. |
| `real_declaring_method.present == false` | Rule 1: CompatibilityShim. Keep `SyntheticStub`; it will be refused under `JdkOnly`, which is the intent. |
| `acc_native == true` | Rule 2: candidate `Bridge`. Still run `jdk-only-native-review.md` §4's four completeness dimensions before promoting. |
| `has_code == true` and the callback is a re-implementation | Rule 4: `SyntheticStub`, so real bytecode wins. Promote to `Intrinsic` only with a parity proof *and* a measurement. |

### Step 3 — make the change per subsystem, never globally

One coherent subsystem per PR, per `jdk-only-native-review.md`'s PR discipline.
That rule exists because the 2026-07-14 change was global and therefore
unattributable, and the only recovery was a full revert.

Prefer a **narrow inner scope** over editing an outer `set_category`:

```rust
// Good: states a judgement about these registrations only.
registry.with_category(NativeKind::SyntheticStub, |r| {
    r.register("java/awt/Component", "setBounds", "(IIII)V", …);
});

// Bad: changes the tag of every callee in the dynamic call tree.
registry.set_category(NativeKind::SyntheticStub);
```

Never move an existing `set_category` line up or down to fix a tag. That
changes the category of every registration between the old and new positions,
and there is no compile error and no test that will tell you.

### Step 4 — prove nothing moved

```bash
cargo test -p cratonvm-native-builtins --test stub_ratchet -- --nocapture
cargo test -p cratonvm-native-api --test registry_contracts -- --nocapture
cargo test -p cratonvm-vm --test synthetic_diff -- --nocapture
CV="$PWD/target/release/cratonvm" JDK="$JAVA_HOME" bash regression-suite/run.sh
cargo run -p cratonvm-difftest --bin cratonvm-difftest -- \
  gate --jdk "$JAVA_HOME" --corpus difftest/seeds
```

Diff the before/after `--dump-native-registry` and attach both fragments. If a
stub was removed, lower `BASELINE_SYNTHETIC_STUBS` in
`native-builtins/tests/stub_ratchet.rs` in the same change. Never raise it to
make a build green.

### Step 5 — suggested order

1. `native-awt`. 122 registrations, one file, an existing conformance test
   (`vm/tests/t7_desktop_conformance.rs`), and the clearest evidence. Split
   `register_toolkit_natives`, `register_headless_natives` and
   `register_image_natives` into a bridge half and a stub half; retag the other
   seven groups wholesale.
2. `native-io`'s four confirmed bridges. Convert the inherited `Bridge` on
   `register_sun_nio_ch_net`, `register_process_natives`,
   `register_random_access_file_natives` and `register_nio_natives_real` into an
   explicit per-function statement so they stop depending on
   `register_io_natives`'s ambient value. This is a no-op on behaviour and it
   pins the four registrars that most need pinning.
3. `native-io`'s five confirmed stubs (`scanner`, `data_stream`, `string_rw`,
   `stream_decoder`, `stream_encoder`). Behaviour-affecting under strict mode;
   needs the charset/`sun.nio.ch` gap closed first.
4. §5.1's four candidates in `native-builtins` and `vm`, one PR each. The
   `register_stamped_lock_natives` overwrite is the cheapest and most
   instructive.
5. `native-collections`. Largest and last. 1,195 registrations, 214 of which
   intercept abstract interface methods and therefore user classes; this is the
   one that can break Spring, Hibernate and Elasticsearch at once. Subdivide by
   collection family, one PR per family.

---

## 8. Should `register()` take an explicit kind?

**Yes — but not by adding a fifth parameter to `register()`.**

### 8.1 The case for change

Every defect in this document is a defect of *dynamic scope*. The kind of a
registration is not visible at the registration; it depends on the call chain,
on statement order within a function, and on which of several callers ran last.
That produces four failure modes, all observed here and none catchable by the
compiler:

1. Registration before the `set_category` line in the same function (§5.2, row 1).
2. Registration after a `set_category(__prev)` restore.
3. A registrar called from two places at two categories (`register_stamped_lock_natives`).
4. A function-pointer `with_category` extending over an entire call tree
   (`native-awt`).

An explicit kind makes all four impossible to express. It also converts the
audit this document performed by hand into `rg`.

### 8.2 The case against a signature change

`register()` is called **~14,800 times** in this repository:

| Crate | call sites |
|---|---:|
| `native-builtins` | 11,709 |
| `native-collections` | 1,219 |
| `native-io` | 1,129 |
| `vm` (mostly test fixtures) | 562 |
| `native-awt` | 122 |
| `native-api` (mostly test fixtures) | 84 |
| `native-builtins-security` | 3 |
| **Total** | **~14,828** |

(Static count of `.register(` in the crates that register natives, taken at the
evidence date. It includes `#[cfg]`-gated and test code and is an upper bound.)

A required fifth parameter is a ~14,800-call-site mechanical edit, and it cannot
be done mechanically **correctly**: a script can only fill in the category the
ambient analysis *believes* is in effect, which would freeze all of today's
mis-tags into explicit source and destroy the evidence that they were never
chosen. It would also collide catastrophically with concurrent work — this
audit alone watched one agent's `registry.rs` changes disappear under a merge.

### 8.3 Recommended shape

A three-stage migration that gets the guarantee without the flag day.

**Stage 1 — add an explicit form; change nothing else.** Zero call sites move.

```rust
/// Register `callback` with an explicit kind, independent of `current_category`.
/// Prefer this over `register()`; the ambient category is legacy.
#[track_caller]
pub fn register_as(
    &mut self,
    kind: NativeKind,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    callback: NativeCallback,
);
```

`register()` keeps its signature and delegates with `self.current_category`.
`#[track_caller]` on both is what contract §4 already requires for
`registered_by`, and it is what makes an ambient tag attributable to a line even
before it is migrated.

**Stage 2 — migrate by subsystem, using the wave-2 procedure above.** Each PR
converts one subsystem's registrars from ambient to `register_as`, *after* the
census has said what the kind should be. Migration and classification happen in the same
change, which is the only way the result is trustworthy. The practical unit of
work is a category **scope**, not a call site — there are 578 of them
(561 `set_category(NativeKind::…)` plus 17 `with_category`, excluding the 557
`set_category(__prev)` restores that pair with them): 485 in `native-builtins`,
49 in `native-collections`, 36 in `native-io`, 7 in `vm`, 1 in `native-awt`.
Two orders of magnitude smaller than 14,800.

**Stage 3 — remove the ambient API.** Once no registrar depends on it, delete
`set_category`, `with_category` and `current_category`, and make `kind` a
required parameter of `register()`. At that point the change is textual and safe
because every remaining call already states its kind.

**Interim hardening, cheap and available now.** Change the default
`current_category` from `SyntheticStub` to a distinct
`NativeKind::Unclassified`, allowed in `Compatible` mode and refused in
`JdkOnly` exactly as `SyntheticStub` is today. Behaviour under `--real-jdk` is
unchanged and behaviour under `--jdk-only` is unchanged, but the census gains
the ability to distinguish "someone decided this is a stub" from "nobody
decided anything", which no artifact can currently express. That single change
would have made this entire audit a `jq` query.
