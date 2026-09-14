# W4-4 — the "synthetic slot index on a real JDK object" species: sweep record

Status: two MISMATCHes fixed, the rest of **this lane's enumerated surface**
verified safe with a reason per site. Wave 4, lane W4-4.

> **2026-08-12 (SECOND PASS) — this record's value is its CENSUS, and the census
> is stale in the good direction. Read this note before quoting any number
> below, including the numbers in the note under it.**
>
> Two of the three instrument holes every table here is bounded by have since
> been closed, so the figures below are a *snapshot of a narrower instrument*
> rather than a measurement of the species.
>
> 1. **The detector is no longer on one funnel.** It lives in
>    `native-api/src/layout_alias.rs` with two observation points —
>    `NativeContextImpl::alloc_object` (`vm/src/vm/vm_exec.rs:12058`), which every
>    native object allocation in every native crate reaches, and the fabrication
>    funnel, kept because it clamps `n = requested.max(real)` before allocating
>    and is therefore the only place an `under` request is still visible. One
>    implementation, two callers. The note below this one says the "intersect list
>    1 with lists 2 and 3" procedure has a list 1 that cannot contain
>    `AsynchronousSocketChannel`; **that is no longer true**, and the procedure in
>    "Visible, not fatal" is now sound as written.
> 2. **`declared == 0` is no longer silent.** It was `None` — the same bytes on
>    the wire as clean — and is now `direction = "undeclared"`
>    (`W7-73-short-object-blind-spot.md`). This matters more than it sounds:
>    the base allocator clamps two lines *after* it observes, so `under` describes
>    a mis-request and **never a short object**, and `undeclared` is the only
>    direction in which a genuinely short object can appear. The `real == 0`
>    column below — 152 pairs / 339 sites here, 197 / 447 in W7-49 — was the
>    "unmeasured, not cleared" bucket and is now reportable.
> 3. **Still open, and it bounds everything:** no figure in this file or in W7-49
>    has ever been produced by a *run*. They are `javap`-plus-source upper bounds.
>    The commands that produce the real numbers are in the closing section of this
>    note's parent record and in W7-49 §9.4.
>
> The source-level counts that ARE current, because they are held by gates rather
> than by prose: the `ClassId::new(0)` fallback population is **28** sites, 14 of
> them naming a class whose real layout is wider (so those objects are short),
> ratcheted downward-only by
> `native-api/tests/layout_alias_coverage.rs::the_unresolved_class_fallback_population_only_shrinks`.
> Everything else in this file needs the run.

> **2026-08-12 — re-censused against the un-blinded detector; the status line
> above is narrower than it reads, and three paragraphs below are wrong on the
> facts.** `W7-49-slot-index-recensus.md` re-derives this census with
> `report_layout_alias` reporting both directions, and adds the census this one
> never took: not how wide each object was allocated, but which slot INDICES are
> written past a class's declared width (174 such writes, 23 classes). Four LIVE
> sites repaired there. The corrections that land on this document:
>
> * **The repair is genuinely present** — verified in source, not taken on
>   trust — but the census it enables is still partial in a way not recorded
>   here: it sits on ONE funnel, and **511 direct `alloc_object` call sites in
>   the native crates never reach it**. The widest LIVE over-allocation in the
>   workspace (`AsynchronousSocketChannel`, 4 slots against a class declaring 1,
>   allocated by `native-io/src/async_socket.rs`) is invisible to this
>   instrument, so the "intersect list 1 with lists 2 and 3" procedure the
>   closing section prescribes has a list 1 that cannot contain it.
> * **"Only slots 0 and 1 are ever written"** (the `CompletableFuture` paragraph
>   under "Visible, not fatal") is false for the workspace:
>   `phases_late/concurrent.rs` writes slot 2 at 39 sites and `http_client.rs`
>   wrote slots 2 and 3 on a LIVE essential-path native. The paragraph's
>   conclusion survives; its premise does not.
> * **The "Latent, not currently live" `HashSet` paragraph** names four sites.
>   The population is 20 allocation sites across 14 files, three of them LIVE.
> * **The `register_p67_async_channels` row** below is a right verdict for a
>   wrong reason, and it is the fifth row in this document to be that. "The
>   receiver is always CratonVM-fabricated" does not make a slot write safe — the
>   object still carries the REAL class's `ClassId`, so slot 0 is `provider`, a
>   reference the collector scans, whoever allocated it. What actually makes the
>   row harmless is that seven of its eight triples are **dead**: `native-io`
>   registers the same class later and registration is last-write-wins. The
>   surviving eighth uses a slot map that disagrees with its owner's on the
>   meaning of slots 0, 1 and 2.
> * `java/util/concurrent/ConcurrentHashMap` is **16 vs 12**, not 16 vs 10 — the
>   two `java.util.AbstractMap` fields are inherited and count.

> **2026-08-11 — the instrument this record's closing paragraph prescribes was
> blind in the direction that matters.** `report_layout_alias` was reporting
> only under-allocation; over-allocation, the direction §5 of
> `docs/architecture/natives-over-real-jdk-classes.md` calls heap corruption,
> was silent. It now reports both. **What every prior lane read as a clean
> census over this funnel was a half census.** See "The detector was blind"
> below, which also carries the first crate-wide requested-vs-declared count
> and corrects one row of W7-9 §5.

## The species

A native reads or writes a real JDK object's field **by slot index**, using the
index the *synthetic* layout would have. It fails silently: the wrong field
reads back as empty, zero or null and the caller takes a wrong branch with no
exception. Six prior instances (`read_module_name`, `parse_nestmate_option`,
`register_re7_datagram_socket`, `Security.getAlgorithms`, `alloc_lookup_for`,
`build_string_set`'s cousin in `nio_file`) are all now fixed.

Remedy: resolve by NAME first (`get_field_by_name` /
`resolve_field_index_by_class_id`), keep the slot index as the synthetic
fallback. The discriminator needs no mode flag — a fabricated stub names its
fields `_f0.._fN`, so a by-name resolve misses there and falls through.
`net_phase_e.rs::dp_layout` is the reference implementation.

## The reachability rule (this is what bounds the sweep)

> **CORRECTED 2026-08-07 — this rule is too restrictive, and door 2 does not
> exist as stated. The sweep it bounds is therefore too narrow, not too wide.**
> The full correction is
> [§1 of *Natives over real JDK classes*](../../architecture/natives-over-real-jdk-classes.md);
> the short version:
>
> * **On the cold interpreter paths, registration itself is the gate** — a
>   native registered for the triple beats real bytecode with no list consulted.
>   `native_override.rs`'s module banner says so outright (*"A registered native
>   wins over real bytecode unconditionally. The predicates decide which methods
>   are registered as overrides, not whether an override applies once it
>   exists."*), and `resolve_step1_native` hard-codes `compat_native_wins = true`.
> * **`resolve_dispatch` is not the live decider.** Its only non-test caller sits
>   inside an `if is_native {` block, and its step 1 returns unconditionally for
>   `method.is_native()` — so steps 2–4, including the `Intrinsic` step, execute
>   only from `vm/tests/jdk_only_dispatch.rs`.
> * **`NativeKind` only ever subtracts.** On the live adapter the kind is read
>   after `compat_native_wins` and is discarded entirely in `Compatible` mode.
>   `Intrinsic` buys exemption from the `--jdk-only` yield
>   (`policy.is_jdk_only() && bytecode_available && kind != NativeKind::Intrinsic`),
>   JIT direct-bind approval, and survival of two registration-time drop arms —
>   never precedence it did not already have.
> * Doors 1, 3 and 4 survive. Door 3 (`force_native_over_real_jdk_bytecode`) and
>   `vm_exec.rs`'s `check_override` chain are what **reinstate** the default on
>   the warm/cached/reflective/JIT paths, which would otherwise prefer bytecode.
>
> **RE-VERIFIED 2026-08-07 (lane W8-7), and the correction above needs one more
> clause: it names the wrong FIRST question.** Before "does a list admit this
> native" comes *"did the registrar that registers it ever run in this mode"*,
> and after it comes *"does this mode refuse the registration"*. The full
> three-gate procedure, the per-path table, and the re-audit of every site this
> campaign called dead are in the retired
> `true-native-precedence-rules-and-the-dead-site-re-audit` write-up (RETIRED
> 2026-08-11: every prediction in it was executed, and its five snippets are now
> the scheduled vectors `RJdkStampedStamps`, `RJdkLookupIn`, `RJdkDefineClass`
> and `RJdkX509Intercept`). **Row 7 of its re-audit is this record's
> `X509Certificate` row, and it is now measured**: sixteen of the seventeen
> registered triples are declared by `sun.security.x509.X509CertImpl` itself, so
> its own bytecode wins; the seventeenth, `getType()`, IS intercepted through
> the superclass walk and answers the constant `"X.509"`, which is what
> `Certificate.getType()` returns for every X.509 certificate.
> Two clauses of the correction above are load-bearing and now confirmed against
> a third path each lane missed:
>
> * `invoke_on_class_shared_inner`'s ~302-disjunct `check_override` chain is
>   **not** that path's gate either. When the chain declines, the bytecode arm
>   ends at a SECOND, unconditional `native_methods.find(declaring_class, …)`
>   (`vm/src/vm/vm_exec.rs:24595-24622`, `override_cb`), vetoed only by the
>   interface-default guard and the synthetic-stub yield. So reflection,
>   `ctx.invoke_virtual`, lambda method-refs and JNI all get "registration is the
>   gate" too.
> * The one path where the force list really is the *only* door is the warm
>   vtable-cached one (`dispatch_virtual.rs:741-779`): a `CachedBytecodeMethod`
>   consults the registry solely through `force_native_over_real_jdk_bytecode`.
>   That asymmetry is a live divergence source, not a theoretical one — see
>   `invoke.rs:3526-3542`, whose guard exists because an interface bridge ran on
>   the first, cold call at a site and the bytecode ran on every later one.
>
> **What this means for the sweep below:** any site excluded because "the method
> has `Code` and is neither `Intrinsic` nor force-listed" was excluded on a false
> premise and needs re-checking. The verdicts in the table are not invalidated —
> a site marked SAFE for a layout reason is still safe — but the *set of sites
> considered* is, and four rows below state a **wrong reason for a right
> verdict**; they are corrected in place and marked *(reason corrected
> 2026-08-07)*.

~~A native in `native-builtins` can only *see* a real JDK receiver through one of
four doors. `vm/src/vm/vm_exec.rs::resolve_dispatch` decides:~~

1. **`ACC_NATIVE` JDK method** — no bytecode exists, so the registration always
   answers. (`ClassLoader.defineClass2`, `Module.addExports0`, `Perf.*`, …)
2. ~~**`NativeKind::Intrinsic`** — the one kind allowed to shadow bytecode.~~
3. **`force_native_over_real_jdk_bytecode`** (vm/src/runtime/interpreter/
   native_override.rs) — an explicit per-triple list.
4. **no `Code` anywhere in the hierarchy** — an interface/abstract declaration
   that nothing overrides.

~~Everything else loses to real bytecode: *step 3 of `resolve_dispatch` —
"real class bytes are authoritative"*.~~ And the whole
`register_phase50..72_natives` family is reached ONLY from
`register_synthetic_overrides`, which `vm_init.rs:1552` calls **only when
`config.use_synthetic_jdk` is true at runtime** — not merely when the Cargo
feature is on. So a registrar whose only caller chain is
`register_phaseNN_natives` cannot see a real object at all.

The real-JDK-reachable set from this lane's files is therefore just the
registrars called from `register_essential_natives_with_shims`:
`lang_system::register_runtime_natives`, `register_synthetic_stream_spliterators`,
`register_p68_crypto_mac`, `register_p71_biginteger_extras`,
`register_p67_async_channels`, the `native_module_*` fn-pointers,
`register_p59_stackwalker`, `register_p71_thread_extras`,
`register_phase57_process`, `jboss_jdkspecific::register_jboss_jdkspecific`,
`register_p67_foreign_memory`, `register_t19_k3_forkjoinpool_common`,
`jdbc::register_p68_jdbc`, `register_real_jdk_stackwalker_frame_method_type`,
`register_real_jdk_charset_contains`, `register_real_jdk_files_owner`,
`register_p68_security_cert`, `register_p69_cleaner`, `register_new15_loom`,
`register_p62_char_buffer`, `register_p66_file_visitor`, plus the ~30
`lang_system` fn-pointer natives.

## MISMATCH 1 — `ByteBuffer` in `ClassLoader.defineClass(String, ByteBuffer, PD)`

`native-builtins/src/lang_system.rs::read_byte_buffer_define_class_slice`.

Door 1: `ClassLoader.defineClass2` is `private native` in the real JDK, and
`lang_system::native_classloader_define_class2` is registered on the essential
path, so the receiver is *always* a real `java.nio.ByteBuffer`.

The code read the backing array at slot 0.

    javap -p java.nio.Buffer      → mark(0) position(1) limit(2) capacity(3) address(4) segment(5)
    javap -p java.nio.ByteBuffer  → hb(6) offset(7) isReadOnly(8) bigEndian(9) nativeByteOrder(10)
    javap -p java.nio.HeapByteBuffer → declares NO instance fields of its own

So slot 0 on a real heap buffer is **`mark`, an `int`**. The `if let
Value::Object(Some(array))` never matched, execution fell into the
direct-buffer branch, `address` read back 0, and the call threw
`ClassFormatError: defineClass2: direct ByteBuffer has no native address` for a
perfectly valid heap buffer. `position`/`limit`/`capacity` at 1/2/3 coincide
with the real layout; `hb` and `offset` do not.

Fixed with a `bb_define_layout` helper in the `dp_layout` shape. It also now
honours `ByteBuffer.offset` (non-zero for anything from `slice()`), which the
old code ignored entirely — bounds are checked in buffer-relative coordinates
and translated to the absolute `hb` index at the end. On a fabricated stub the
by-name resolve misses and the old {0,1,2,3} layout is used unchanged, so the
existing `define_class2_reads_bytebuffer_backing_array` unit test (whose mock
answers `None` from `resolve_field_index_by_class_id`) is unaffected.

## MISMATCH 2 — `StackWalker.StackFrame.getMethodType()` / `getDescriptor()`

`native-builtins/src/phases_late/reflect_invoke.rs::p59_sf_get_method_type`.

Door 3: `native_override.rs:2635-2636` force-routes
`("java/lang/StackWalker$StackFrame", "getMethodType")` and `("…", "getDescriptor")`
over real bytecode, and `register_real_jdk_stackwalker_frame_method_type` binds
both on the **essential** path.

Two different carriers reach that one function:

* `reflect_invoke::populate_stack_frame` — the synthetic 8-slot
  `java/lang/StackWalker$StackFrame`: slot 1 = methodName, slot 5 = declaring
  class internal name.
* `lang_stackwalker::populate_sfi` — a **real** `java.lang.StackFrameInfo`:

      javap -p java.lang.ClassFrameInfo  → classOrMemberName(0) flags(1)
      javap -p java.lang.StackFrameInfo  → name(2) type(3) bci(4) contScope(5) ste(6)

Slot 5 holds on both — `populate_sfi` deliberately stashes the '/'-form
internal name in `contScope` (its own comment says so). **Slot 1 does not**: on
a real `StackFrameInfo` it is the `int flags` word. `read_string` refused it,
`method_name` came back empty, and `getMethodType()` answered **null** while
`getDescriptor()` threw `UnsupportedOperationException: StackWalker frame
descriptor metadata is unavailable` — for every real-JDK frame.

Fixed by reading `name` by name first with the slot-1 fallback, the same order
the sibling `StackFrameInfo.getMethodType()` in `lang_stackwalker.rs` already
used. (`p59_sf_get_method_type_retain_checked` was already correct: it consults
`class_frame_retains_class_ref`, a by-name `flags` read, before falling back to
slot 7.)

The neighbouring `getClassName`/`getMethodName`/`getFileName`/`getLineNumber`/
`getByteCodeIndex` accessors on the same interface are **safe** even though
their slot mapping is equally wrong for a real `StackFrameInfo`: those triples
are NOT force-listed, `StackFrameInfo` declares each of them with `Code`, and
step 3 gives the real bytecode precedence. Adding any of them to
`force_native_over_real_jdk_bytecode` without also fixing their slot reads
would reintroduce the bug at five sites at once.

## Verified safe (with the reason)

| site | class | real instance reachable? | `javap` layout | verdict |
|---|---|---|---|---|
| `lang_system` `native_thread_get_name` | `java.lang.Thread` | yes | name(2), slot 0 = `eetop` | safe — by-name first, slot 0 is the fallback |
| `lang_system` `exec_dir_path` | `java.io.File` | yes | path(0) | safe — by-name first; slot 0 happens to match |
| `lang_system` `install_charset` | `java.nio.charset.Charset` | yes (raw `alloc_object` of the real class) | name(0) aliases(1) aliasSet(2) | safe — slot 0 IS `name` |
| `lang_system` `native_pb_init`/`_command` | `java.lang.ProcessBuilder` | **YES** — *(reason corrected 2026-08-07)* "has `Code`, not force-listed" does not exclude anything; `ProcessBuilder` is concrete, so a registration on that exact name wins on every cold and every reflective dispatch | command(0) directory(1) environment(2) | safe **only** because the indices match the real layout — the reachability half of the old argument was false |
| `lang_system` PD/CodeSource reads in `defineClass1/2` | `java.security.ProtectionDomain`, `CodeSource` | yes (door 1) | PD codesource(0); CS location(0) | safe — both slot-0 reads hit the intended field |
| `lang_system` `native_runtime_version*`, `native_thread_get_state` | `Runtime$Version`, `Thread` | yes (force-listed) | — | safe — every read is by name |
| `lang_system` env-`HashMap` builder, `wrap_system_env_map` | `HashMap`/`HashMap$Node`, `cratonvm/internal/UnmodifiableMap` | — | — | safe — real path resolves every index by name; fallback path uses `ensure_synthetic_class` |
| `lang_system` slots at 4712-4715 / 5081 | mock objects | — | — | safe — `#[cfg(test)]` |
| `phases_late` `register_phase57_process` PB block | `java.lang.ProcessBuilder` | no (see above) | command(0) directory(1) environment(2) | safe; `PB_FIELD_*` match the real layout |
| `phases_late` `ProcessHandle.pid` / `p60_handle_pid` / `compareTo` | `java.lang.ProcessHandleImpl` | yes | pid(0) startTime(1) | safe — slot 0 IS `pid`, and it is a `long` |
| `phases_late` `ProcessBuilder$Redirect.PIPE/INHERIT` | `ProcessBuilder$Redirect` | no | no instance fields at all | safe — `PIPE`/`INHERIT` are static *fields* in the real JDK, so no such method is ever resolved |
| `phases_late` `register_p60_process_handle` (children/descendants/parent/info) | `ProcessHandle` | no | — | inert: only caller chain is `register_phase60_natives` → `register_synthetic_overrides` |
| `phases_late` `register_p69_cleaner` `create` | `java.lang.ref.Cleaner` | possible | impl(0) | safe — explicitly guarded by `is_class_synthetic_stub("java/lang/ref/Cleaner")` |
| `phases_late` `register_p71_biginteger_extras` | `java.math.BigInteger` | possible | signum(0) mag(1) | safe — every read/write goes through `bi_layout()`, which resolves by name |
| `reflect_invoke` `register_p59_stackwalker` slots 0-7 | `StackWalker$StackFrame` | no (see MISMATCH 2 discussion) | — | safe |
| `reflect_invoke` `read_module_name`, `native_module_can_read/add_exports/add_opens/impl*` | `java.lang.Module` | yes (door 1 + force-listed) | layer(0) name(1) loader(2) descriptor(3) | safe — already fixed; by-name first |
| `reflect_invoke` `build_string_set` (HashSet slots 0/1/2) | `java.util.HashSet` | no | **map(0) is its ONLY instance field** | inert — sole caller is `register_p59_module`'s `Module.getPackages`, and that registrar is reachable only from `register_synthetic_overrides`. **Shape is wrong for a real HashSet** — see "latent" below |
| `charset_buffers` `register_p62_char_buffer`, `cb_write_hb`/`cb_read_hb`/`cb_set_mark` | `java.nio.CharBuffer` | yes | Buffer mark(0)…address(4), then hb/offset/isReadOnly | safe — already swept; indexed writes are gated on `cb_synthetic_layout` and `address` is re-asserted by name |
| `charset_buffers` `register_real_jdk_charset_contains` | `java.nio.charset.Charset` | yes (force-listed) | — | safe — no slot access; goes through `invoke_virtual("name")` |
| `nio_file` `register_real_jdk_files_owner`, `register_p66_file_visitor` | `Files`, `SimpleFileVisitor` | yes / no | — | safe — no receiver slot access in either |
| `foreign_ffm` `p67_segment_byte_size`/`p67_segment_address` | `jdk.internal.foreign.AbstractMemorySegmentImpl` | yes (force-listed) | `length`, `min` | safe — by-name first, slots are the fallback |
| `ssl_security` `register_p68_crypto_mac` | `javax.crypto.Mac` | yes | — | safe — no receiver slot access |
| `ssl_security` `register_p68_security_cert` (slots 0-3) | `java.security.cert.X509Certificate` | **per-method** — *(reason corrected 2026-08-07)* "none is force-listed" excludes nothing. The real gate is that `X509CertImpl` **declares** each triple, so the dispatch class name misses the registry and step 1's superclass walk is skipped (`has_own_bytecode`). Any triple the impl does *not* declare walks up and hits the abstract-class registration | — | safe for the triples `X509CertImpl` overrides; **unverified** for any it inherits |
| `net_channels` `register_p67_async_channels` (slots 0-3) | `java.nio.channels.Asynchronous*Channel` | no | — | safe — abstract classes; the static `open()` factories have `Code`, so the receiver is always CratonVM-fabricated |
| `jdbc` `register_p68_jdbc` (~130 slot sites) | `java.sql.Connection/Statement/PreparedStatement/ResultSet/CallableStatement/DatabaseMetaData` | no | — | safe — *(reason corrected 2026-08-07)* not "step 3", which never runs. Two real gates: the driver class (`org.h2.jdbc.*`) is the declaring class, so the registry lookup is keyed on a name with no registration; and the interface-default guard (`invoke.rs:3550`, `vm_exec.rs:24595`) drops interface-name natives for instance calls outright |
| `streams` `register_synthetic_stream_spliterators` | — | — | — | safe — no receiver slot access |
| `jboss_jdkspecific` `module_registry_name`, `build_module`, `native_module_define_module0` | `java.lang.Module` | yes (door 1) | layer(0) name(1) loader(2) descriptor(3) | safe — already fixed; `build_module` carries an explicit "NB: no raw slot write here" note |
| `jboss_jdkspecific` `wrap_optional_present` | `java.util.Optional` | possible | value(0) — its only instance field | safe — slot 0 IS `value` |
| `jboss_jdkspecific` slots at 1812 / 1861 | — | — | — | safe — `#[cfg(test)]` |
| `concurrent.rs` `register_p71_thread_extras`, `register_new15_loom`, `register_t19_k3_forkjoinpool_common` (not this lane's file) | `Thread`, Loom, `ForkJoinPool` | yes | — | safe — **zero** receiver slot accesses in all three bodies |

`java.util.ArrayList` deserves a note because the pattern recurs across these
files and looks wrong but is not: `set_field(list, 0, Int(0)); set_field(list, 1,
array); set_field(list, 2, Int(0))` maps onto `AbstractList.modCount(0)`,
`ArrayList.elementData(1)`, `ArrayList.size(2)` — `AbstractCollection` declares
no instance fields. The convention in this tree is real-layout-compatible.

## Latent, not currently live

`reflect_invoke::build_string_set` builds a `java.util.HashSet` as
`{ slot 0 = String[], slot 1 = size, slot 2 = capacity }`. A real `HashSet` has
exactly one instance field, `map:HashMap`, so this is the same shape as the
already-fixed `Security.getAlgorithms` bug: real `HashSet.contains()` bytecode
would run `map.containsKey(o)` against a `String[]`. It is inert today only
because its sole caller is synthetic-jdk-mode-only. **Anything that promotes
`Module.getPackages` to the real-JDK path must switch it to
`crate::build_real_layout_string_hashset` first** — the remedy helper already
exists and `nio_file.rs:1043` documents the same trap for
`FileSystem.supportedFileAttributeViews`. The same caveat applies to
`reflect_invoke.rs:2443` (`ModuleLayer.modules()`) and to the `HashSet`
3-slot allocations in `collections.rs:86,1178` and `text_intl.rs:1525`.

> ### "INERT" IS WRONG — measured 2026-08-12 (lane A31)
>
> The `build_string_set` row in the table above and this section both read
> **"inert"**, on the reasoning that the sole caller (`register_p59_module`'s
> `Module.getPackages`) is "reachable only from `register_synthetic_overrides`".
> The reachability half is right. The *inert* half is a scope claim being read
> as a harmlessness claim, and it is false: a `--features synthetic-jdk` binary
> was launched with `--synthetic-jdk` and the caller runs, every time, and
> **answers empty**.
>
> ```
>                                  HotSpot 25                       --jdk-only                --synthetic-jdk
> R module.getPackages =           cls=…ImmutableCollections$SetN   cls=java.util.HashSet     cls=java.util.HashSet
>                                  size=196 hasJavaLang=true        size=63 hasJavaLang=true  size=0 hasJavaLang=false
> R module.getPackages.iterate =   6                                6                         0
> R module.unnamed.getPackages =   cls=java.util.HashSet size=1     size=63                   size=0
> R module.getName =               java.base                        java.base                 null
> R module.isNamed =               true                             true                       false
> R moduleDescriptor.name =        name=java.base                   name=java.base            name=null
> R module.layer =                 <62 modules>                     java.base                 null
> ```
>
> `getPackages()` on `java.base` returns an **empty** `HashSet`, with no error
> and no violation. That is the `W7-1`/`W7-20` failure mode — an empty
> collection reads as a pass anywhere the caller only iterates — and it is
> reached, not latent. `getDescriptor()` is non-null but its `name()` is `null`,
> and `isNamed()` is `false` for `java.base`, so the module identity surface is
> answering three mutually inconsistent things at once.
>
> The 3-slot `HashSet` shape hazard this section is *about* is **not** what the
> run exposed — nothing crashed and no `contains()` misfired, because the set is
> empty and nothing was looked up in it. The shape stays a real latent hazard for
> any future promotion to the real-JDK path, exactly as written. What changes is
> the priority framing: this is not a dormant landmine, it is a live wrong answer
> in the mode it ships in.
>
> **Restate the row as:** *reachable only from `register_synthetic_overrides`;
> LIVE and answering empty in `--synthetic-jdk`; shape is also wrong for a real
> HashSet, which matters only if it is ever promoted.*
>
> Not adjudicated by this lane: the three sibling allocations
> (`reflect_invoke.rs:2443` `ModuleLayer.modules()`, `collections.rs:86,1178`,
> `text_intl.rs:1525`). `ModuleLayer.modules()` is implicated by
> `module.layer = null` above but was not isolated.

## A different species, found in passing (GC stale local)

Two sites held a freshly-allocated, unrooted `ObjectRef` in a bare Rust local
across a later allocation, then stored it — a use-after-move under the moving
collector. Both fixed with the established pin idiom
(`pin_native_root` → alloc → `read_native_pin` → `unpin_native_roots`):

* `phases_late.rs::p60_process_parent` — `parent` held across the `Optional`
  allocation.
* `phases_late.rs` `ProcessHandle$Info.command()` — `text` held across the
  `Optional` allocation.

Two more of the same shape remain in `reflect_invoke.rs`'s
`register_p59_module` (lines ~2471 and ~2498: `m_obj` held across
`create_string`). Left alone — that registrar is synthetic-jdk-only and the
change belongs with whoever next touches it.

## The detector was blind in the dangerous direction (2026-08-11)

Branch `fix/jdk-only-slot-index-detector-blindness-20260811`. Nothing here is
built or run; every field count is `javap -p` against the JDK 25.0.3.9 image on
the Windows host (`C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot`),
`javap -version` = `25.0.3`.

### What the condition was, and what it is now

`native-builtins/src/util_concurrent_ext.rs::try_alloc_concurrent_synthetic`,
before:

```rust
let real = ctx.class_num_total_fields(cid);
if num_fields > 0 && num_fields < real {
    report_layout_alias(class_name, num_fields, real);
}
let n = num_fields.max(real);
```

after:

```rust
let real = ctx.class_num_total_fields(cid);
if num_fields > 0 && real > 0 && num_fields != real {
    report_layout_alias(class_name, num_fields, real);
}
let n = num_fields.max(real);
```

`report_layout_alias` now branches on the direction and emits a `direction =
"under"` / `direction = "over"` field. Same function, same
`CRATONVM_DBG_LAYOUT_ALIAS=1` gate, same `(class, requested, file, line)` dedup
key, same `#[track_caller]` chain — **one channel, not two**, so the intersection
procedure the closing section of this record prescribes still works unchanged and
simply has more to intersect.

### Why the silent direction was the sharp one

The clamp `n = num_fields.max(real)` is what made under-allocation reportable
and survivable at once: the object gets the real width, so every access stays in
bounds and the damage is aliasing — a wrong answer. Over-allocation has no such
floor:

* The object comes back with `num_fields` slots while its class declares `real`,
  so its header disagrees with `num_total_fields`. That is verbatim the condition
  `vm/src/memory/gc.rs::validate_object_sizes` (`CRATONVM_DBG_VALIDATE_NEW=1`)
  prints as `[young-validate] BAD <class> num_slots=N EXPECTED=M`. That validator
  was written for a JIT `new` emitting a wrong-size header (the JUnitCore.main
  miscompile → heap-walk desync); this funnel manufactures the identical shape,
  deliberately, and the two instruments have never been read together.
* The wide slot map is not confined to the objects this funnel made. A request
  of 4 against a class declaring 2 is the caller asserting a four-entry map for
  that class name, and the same natives receive instances they did not allocate.
  `util_concurrent_ext.rs::native_cf_complete` proves the receivers arrive: its
  whole body is a discriminator on slot 1's value *type*, because a real-JDK
  `CompletableFuture` reaches it with `stack` there and a synthetic one with an
  `Int` done-flag. Applied to a real 2-slot object, indices 2 and 3 are past the
  end.

So the pre-existing state of this instrument was the campaign's other recurring
failure mode, not a bug in a native: an instrument that cannot report the thing
it exists to catch reads exactly like a clean measurement. Every earlier lane
that ran `CRATONVM_DBG_LAYOUT_ALIAS=1` and found nothing found nothing about
over-allocation.

### `real == 0` is excluded from both directions, and that is a hole

The new test carries `real > 0`. `class_num_total_fields` returns 0 for two
different facts — "class not loaded yet" (the reason the `max` exists at all)
and "genuinely no instance fields", which is every interface and
`java/lang/Object`. This funnel is asked for interface names routinely; two are
in this very file (`java/util/concurrent/locks/Condition` at 1 slot,
`java/util/concurrent/Flow$Subscription` at 2), where a non-zero request is the
intended fabrication and not an alias. Crate-wide that is **152 distinct
(class, requested) pairs across 339 call sites** which this census can say
nothing about. They are unmeasured, not cleared. Separating the two meanings of
0 needs a `class_is_loaded`-style predicate that does not exist on
`NativeContext` today.

### The three named instances, confirmed — and one of them is misfiled

Reported to this lane as three silent over-allocations. Two confirm; the third
is the opposite direction and was already being reported by the old condition.
Counts are total instance fields including inherited (CratonVM's
`num_total_fields`: `first_field_index = superclass.num_total_fields`, so the
count is the transitive one), `static` excluded.

| class | requested | declared (`javap -p`, transitive) | direction | where |
|---|---|---|---|---|
| `java/nio/channels/SelectionKey` | 4 | **1** — `private volatile Object attachment` | **over by 3** | `phases_late/net_channels.rs:706`, `servlet.rs:7080` |
| `java/util/concurrent/CompletableFuture` | 4 | **2** — `volatile Object result`, `volatile Completion stack` | **over by 2** | `util_concurrent_ext.rs` ×4, `http_client.rs` ×2 |
| `java/util/concurrent/CompletableFuture` | 3 | 2 | **over by 1** | `phases_late/concurrent.rs` ×12 |
| `java/nio/channels/DatagramChannel` | 5 | **10** — 0 own + 6 `AbstractSelectableChannel` (`provider`, `keys`, `keyCount`, `keyLock`, `regLock`, `nonBlocking`) + 0 `SelectableChannel` + 4 `AbstractInterruptibleChannel` (`closeLock`, `closed`, `interruptor`, `interruptedTarget`) | **UNDER by 5** | `phases_late/net_channels.rs:3253` |

**W7-9 §5's table lists `DatagramChannel` under *"When `num_fields > real` — the
case for every row below"*. It is not.** That row gives no number for the real
side (*"inherited from `AbstractSelectableChannel` / `AbstractInterruptibleChannel`"*),
and the number is 10 against a request of 5 — the under direction, which the old
detector already reported. The other two rows of that table stand. This is the
census methodology warning in this campaign's README landing on a record that
was otherwise careful: the row that was not resolved to a number is the row that
was wrong.

`SelectionKey` is already diagnosed in place, at
`native-builtins/src/phases_late/net_channels.rs:614-631`, by the W7-9 lane,
including its reason for not repairing it (that registrar is synthetic-only, the
live surface is `native-io/src/nio_selector.rs::register_nio_selector_real`, and
a one-sided renumbering only moves the disagreement). **This lane does not
propose a patch there and there is no out-of-file patch to apply** — the fix
needs a lane owning `native-io` and `servlet.rs`, exactly as W7-9 §5 says.

### Every call site in `util_concurrent_ext.rs`, resolved

Call sites, not grep hits: each row is the literal pair actually passed at that
line.

| line | class | requested | declared | direction |
|---|---|---|---|---|
| 2332, 2342 | `java/util/ArrayList$Itr` | 3 | 4 (`cursor`, `lastRet`, `expectedModCount`, `this$0`) | under by 1 — already reported |
| 3375 | `java/util/concurrent/Flow$Subscription` | 2 | 0 — **interface** | unmeasurable (`real == 0`) |
| 3714 | `java/util/concurrent/locks/Condition` | 1 | 0 — **interface** | unmeasurable (`real == 0`) |
| 4789 | `java/util/ArrayList` | 2 | 3 (`AbstractList.modCount`, `elementData`, `size`) | under by 1 — already reported |
| 5203, 5210, 5369, 5424 | `java/util/concurrent/CompletableFuture` | 4 | 2 | **over by 2 — newly visible** |
| 10341 | `…locks/ReentrantReadWriteLock$ReadLock` / `$WriteLock` (via `class_name`) | 1 | 1 (`sync`) each | exact |

Test-only (`#[cfg(test)]` begins at line 8532; a mock `ctx` answers `real == 0`,
so none of these ever reported and none will):
`ReentrantLock` 3 vs **1** (`sync`) ×11 — over by 2; `Semaphore` 2 vs **1** ×5 —
over by 1; `CompletableFuture` 4 vs 2 ×1 — over by 2; `CountDownLatch` 1 vs 1,
`CopyOnWriteArrayList` 2 vs 2, `AtomicInteger`/`AtomicLong`/`AtomicReference`
1 vs 1 — exact.

The four live `CompletableFuture` sites are **not** repaired here. Only slots 0
and 1 are ever written (`FUT_FIELD_RESULT = 0`, `FUT_FIELD_DONE = 1`, in
`native-builtins/src/lib.rs:33625-33626`), so the two extra slots are dead width
rather than a write past the real layout — but they are dead width that made the
object header disagree with its class, and narrowing 4 to 2 is an allocation
change. This lane changed the reporting only, on purpose: the two must not move
in one step, because if the report is wrong the allocation change has already
shipped.

The separate, worse thing sitting under those sites — `FUT_FIELD_DONE = 1`
writing an `Int` into what a real `CompletableFuture` declares as the reference
`stack`, a bogus pointer for the collector — is not new and is already stated in
`native_cf_complete`'s own doc comment (*"clobbered `stack`@1 with the `done`
int"*), which is why that function discriminates on slot 1's value type. It is
the `MethodHandles$Lookup`/`allowedModes` shape from §5 of the architecture
reference, in a fourth place.

### The first crate-wide count, and what it is not

Resolved over `native-builtins/src/**.rs` by extracting the literal
`try_alloc_concurrent_synthetic(ctx, "<class>", <n>)` pair at each call site and
running `javap -p` transitively over the superclass chain for every JDK class
named. **1,543 call sites resolved this way, out of 1,895 total** — the
remaining ~350 pass the class or the count through a constant, a variable or a
multi-line call and are **not in these numbers**.

| direction | distinct (class, requested) pairs | call sites |
|---|---|---|
| **over** (requested > declared) | 48 | 162 |
| under (requested < declared) | 179 | 442 |
| exact | 78 | 320 |
| `real == 0` (interface / no instance fields) | 152 | 339 |
| class not on the JDK 25 image (shims, `org/…`, test fixtures) | 158 | 280 |

Widest over-allocations, by absolute excess: `java/util/Locale` 32 vs 4
(`locale_bootstrap.rs`), `java/lang/invoke/MethodHandle` 17 vs 6
(`lang_invoke.rs`), `javax/net/ssl/SSLContext` 12 vs 3 (`tls.rs`),
`java/lang/Package` 12 vs 4 (`lang_class.rs`), `java/util/HashSet` 8 vs 1
(`spring_startup_bootstrap.rs`),
`java/util/concurrent/ConcurrentHashMap` 16 vs 10 (9 sites across
`classloader.rs`, `classloader_real.rs`), `java/util/Date` 4 vs 2 (23 sites).

**Three things this table is not.** (1) It is not a bug list — see the next
paragraph. (2) It is not what the detector will print: the detector compares
against whatever CratonVM has loaded, so in synthetic-JDK mode a fabricated
class declares exactly `num_fields` and nothing fires. This is the population
that would report **in real-JDK mode with the class loaded**, i.e. an upper
bound. (3) It is not complete, by ~350 call sites and by the 339 `real == 0`
sites.

**A legitimate sub-population is inside the `over` column and must be
subtracted before anyone counts it as a defect count.** `jca/kem.rs`,
`jca/signature.rs`, `jca/key_factory.rs` and `jca/key_agreement.rs` over-allocate
*on purpose*, through `synthetic_base_offset` (27 uses): it asks
`class_num_total_fields` for the real width and appends private slots **above**
it, *"so reference writes never land on a slot the real layout declares with an
incompatible descriptor"*. That is the correct remedy for this species and it
necessarily reports as `over`. The funnel receives one integer and cannot tell
the idiom from a hard-coded wide guess.

### Visible, not fatal — and what a follow-up must measure to go further

Made visible. Not made fatal, and not narrowed: allocation behaviour is
byte-identical (`max` still wins in both directions).

The reason is that the `over` population had never been counted before the table
above, and the table above is an upper bound containing a known-legitimate
idiom. The 49-class / 75-site measurement quoted in `report_layout_alias`'s body
— the one that argued this census stays off by default — was taken while the
function only saw `under`, so it says nothing about `over`. Turning `over` into
a refusal would convert an unknown number of working call sites into
`NoClassDefFoundError` at boot, across a funnel with ~1,900 call sites.

To go further, a follow-up needs three lists off one workload and their
intersections:

1. `CRATONVM_DBG_LAYOUT_ALIAS=1`, filtered to `direction = "over"` — distinct
   (class, site) pairs.
2. `CRATONVM_DBG_VALIDATE_NEW=1`'s `[young-validate] BAD … num_slots=N
   EXPECTED=M` lines — the objects whose header already disagrees with their
   class.
3. The `cratonvm::gc::guard` out-of-bounds field reads, exactly as this record's
   closing section already prescribes for the `under` direction.

A pair in list 1 only, and reachable from `synthetic_base_offset`, is the
intended idiom. A pair in list 1 only, otherwise, is wide-but-unused and can be
narrowed at the call site — cheap and local. A pair in 1 ∩ 3 is a live defect of
this species. List 2 is the cross-check that the funnel, not the JIT, produced
the shape.

Two questions the follow-up must answer that no instrument in the tree currently
can: whether any of these classes is ever allocated at BOTH widths in one run
(the two-layouts-on-one-class condition that made `java.lang.Process` a bug),
and what the ~350 non-literal call sites request. The first needs the census
keyed by class rather than by site; the second needs the count evaluated at the
funnel, which it already is — those sites simply cannot be read from source.

## Confidence, and the observation that would falsify this

> **2026-08-07 (W8-7).** The `use_synthetic_jdk` half of this paragraph is
> correct and is the load-bearing half. The `resolve_dispatch` step-3 half is
> not: that function's steps 2–4 are unreachable in production. Read the
> paragraph as resting on the runtime gate at `vm_init.rs:1552` alone.

Reachability is the whole argument, and it rests on ~~`resolve_dispatch`'s step 3
plus~~ the `use_synthetic_jdk` runtime gate at `vm_init.rs:1552`. **The single
falsifying observation:** a real `java.util.HashSet`, `java.sql.Connection` or
`java.security.cert.X509Certificate` instance arriving at one of the natives
this document calls inert. The cheapest instrument is
`CRATONVM_DBG_LAYOUT_ALIAS=1`, which makes
`util_concurrent_ext::report_layout_alias` name every call site that allocated
a class under a smaller field layout than the real one; intersect that list
with the `cratonvm::gc::guard` out-of-bounds field reads, exactly as that
function's own comment prescribes. A class in both lists has a live defect.

Neither fix is verified at runtime — this lane cannot build or run the VM. Both
are source-level and argued from `javap` against JDK 25.0.3.9.
