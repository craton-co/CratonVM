# W4-4 — the "synthetic slot index on a real JDK object" species: sweep record

Status: two MISMATCHes fixed, the rest of the lane's surface verified safe with
a reason per site. Wave 4, lane W4-4.

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

A native in `native-builtins` can only *see* a real JDK receiver through one of
four doors. `vm/src/vm/vm_exec.rs::resolve_dispatch` decides:

1. **`ACC_NATIVE` JDK method** — no bytecode exists, so the registration always
   answers. (`ClassLoader.defineClass2`, `Module.addExports0`, `Perf.*`, …)
2. **`NativeKind::Intrinsic`** — the one kind allowed to shadow bytecode.
3. **`force_native_over_real_jdk_bytecode`** (vm/src/runtime/interpreter/
   native_override.rs) — an explicit per-triple list.
4. **no `Code` anywhere in the hierarchy** — an interface/abstract declaration
   that nothing overrides.

Everything else loses to real bytecode: *step 3 of `resolve_dispatch` —
"real class bytes are authoritative"*. And the whole
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
| `lang_system` `native_pb_init`/`_command` | `java.lang.ProcessBuilder` | no (all methods have `Code`, PB not force-listed) | command(0) directory(1) environment(2) | safe, and the indices match anyway |
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
| `ssl_security` `register_p68_security_cert` (slots 0-3) | `java.security.cert.X509Certificate` | no | — | safe — abstract class; a real `sun.security.x509.X509CertImpl` overrides every triple with `Code`, and none is force-listed |
| `net_channels` `register_p67_async_channels` (slots 0-3) | `java.nio.channels.Asynchronous*Channel` | no | — | safe — abstract classes; the static `open()` factories have `Code`, so the receiver is always CratonVM-fabricated |
| `jdbc` `register_p68_jdbc` (~130 slot sites) | `java.sql.Connection/Statement/PreparedStatement/ResultSet/CallableStatement/DatabaseMetaData` | no | — | safe — all six are **interfaces**; any real driver class (H2's `org.h2.jdbc.*`) declares the method with `Code` and wins at step 3. The natives answer only for CratonVM's own fabricated `java/sql/*` receivers |
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

## Confidence, and the observation that would falsify this

Reachability is the whole argument, and it rests on `resolve_dispatch`'s step 3
plus the `use_synthetic_jdk` runtime gate at `vm_init.rs:1552`. **The single
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
