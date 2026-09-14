# Capability gate wiring — `native-builtins` and `native-io`

**Status:** gates wired, **enforcement still off by default**.
**Companion to:** `docs/security/native-capabilities.md` (the audit inventory and
the ordered work list this document executes).
**Scope of this pass:** `native-builtins/`, `native-io/`, and this file. The
capability model in `native-api/src/capability.rs` was not touched.

> Every gate below is a no-op in the default configuration. With no policy
> installed for the VM — which is still every configuration, because nothing
> calls `install_capabilities` yet (work-list item 3) — each helper calls the
> exact unchecked operation the call site used before. With a policy in
> `CapabilityMode::Permissive` the check records the use and returns `Ok`.
> Exactly one edit in this pass changes behaviour by default; it is called out
> in §6 and can be reverted on its own.

---

## 1. Where the gates live

`native-builtins/src/capability_gate.rs` is the single adapter between a
native's `&dyn NativeContext` and `native-api`'s `CapabilitySet`. It exists for
three reasons:

1. The `_checked` openers on `FileDescriptorTable` take `&CapabilitySet`, not a
   context. `ctx.vm_capabilities()` returns an `Option<Arc<…>>` and every call
   site would otherwise have to unwrap it by hand — getting the `None` fallback
   wrong in one place is a silent hole.
2. Translating a refusal into the right Java exception is a decision, not
   boilerplate: `SecurityException` for a denial, the site's own `IOException`
   for I/O. `translate_open_failure` makes that decision once.
3. `grep capability_gate:: native-builtins/src native-io/src` is the complete
   list of gated sites, which is what §2 is derived from.

| helper | capability | notes |
|---|---|---|
| `open_read_gated` | `FileRead(path)` | → `open_read_checked` |
| `open_write_gated` | `FileWrite(path)` | → `open_write_checked` |
| `open_read_write_gated` | `FileRead` **and** `FileWrite` | the fd can do either |
| `open_random_access_gated` | `FileRead` (+ `FileWrite` when writable) | not yet used in `native-builtins`; RAF lives in `native-io` |
| `open_tcp_connect_gated` | `Network(dest)` | |
| `open_tcp_listener_gated` | `Network(bind)` | binding is its own authority |
| `open_udp_gated` | `Network(bind)` or `Any` | portless bind fails closed |
| `gate_network` | `Network(addr)` | for sites that bind/connect **without** the fd table |
| `gate_process_spawn` | `ProcessSpawn(program)` | |
| `gate_foreign_upcall` / `gate_foreign_downcall` | `ForeignUpcall` / `ForeignDowncall` | |
| `gate_library_load` | `LibraryLoad(name)` | provided, not yet wired (see §5) |
| `gate_raw_memory` | `RawMemory("Unsafe.rawAddress")` | **memoized** — see §4 |
| `gate_raw_memory_named` | `RawMemory(op)` | unmemoized, for the FFM accessors |

Every helper is `#[track_caller]`, and so are `CapabilitySet::check` and the
`_checked` openers, so the `first_site` in the audit report names the *native*
that performed the operation, not the helper. A unit test asserts that.

---

## 2. Sites wired

### 2.1 I2 — the `java.nio.file` surface (the largest gap)

The audit listed seven line numbers. They were **not exhaustive**: the same
`grep` pattern, re-run, finds **ten** opener calls in
`native-builtins/src/phases_late/nio_file.rs`, in six natives. All ten are now
gated.

| native | old call | capability |
|---|---|---|
| `FileSystemProvider.newFileChannel` (writable) | `open_read_write` | `FileRead` + `FileWrite` |
| `FileSystemProvider.newFileChannel` (read-only) | `open_read_write` **then** `open_read` — *two* calls, the audit counted one | `FileRead` (+`FileWrite` on the first attempt) |
| `Files.newBufferedWriter` (`open_buffered_writer`) | `open_write` | `FileWrite` |
| `Files.newInputStream` / `FileSystemProvider.newInputStream` (`fsp_new_input_stream`) | `open_read` | `FileRead` |
| `FileSystemProvider.newOutputStream` (`fsp_new_output_stream`) | `open_write` | `FileWrite` |
| `RandomAccessFile.<init>(String,String)` read-only | `open_read_write` **then** `open_read` — *two* calls | `FileRead` |
| `RandomAccessFile.<init>(String,String)` writable | `open_read_write` | `FileRead` + `FileWrite` |
| `RandomAccessFile.<init>(File,String)` | `open_read_write` — **not in the audit list at all** | `FileRead` + `FileWrite` |

The three the audit missed are the `.or_else` read-only fallback in
`newFileChannel`, the same fallback in the `(String,String)` RAF constructor,
and the entire `(File,String)` RAF constructor. The lesson is the one the audit
already states about itself: the seven were "found by one grep", and a grep for
`open_read` does not see a call spelled `.or_else(|_| ctx.fd_table().open_read(…))`
on its own line, nor a second constructor overload.

**Read-only fallback semantics.** Where the code opened read+write and fell back
to read-only on failure, a `FileWrite` refusal now takes the same fallback an
`EACCES` would. That is the correct answer — the caller only asked to read — and
it means a `file-read`-only grant makes `FileChannel.open(…, READ)` and
`new RandomAccessFile(f, "r")` work, which is what an operator would expect.

**Exception shape.** A refusal surfaces as `SecurityException`, never as
`NoSuchFileException` / `AccessDeniedException` / `IOException`. This matters:
callers legitimately catch and recover from the typed `java.nio.file`
exceptions (Spring's `FileSystemResource.readableChannel()` catches
`NoSuchFileException`), and a policy refusal must not be swallowed by that
recovery. I/O failures keep their exact previous messages.

### 2.2 I6 — server socket bind, and the half of it the audit did not name

The audit named four `open_tcp_listener` sites in
`phases_late/net_channels.rs`. All four are gated. But `java.net.ServerSocket`
has a **second** bind path in the *same two natives* that never touches the fd
table at all — it binds a `std::net::TcpListener` directly and files it via
`servlet::s2_alloc_listener`. Switching the fd-table calls to `_checked` would
have left that path wide open, so it is gated with `gate_network`.

| file | native | mechanism |
|---|---|---|
| `phases_late/net_channels.rs` | `ServerSocketChannel.bind(SocketAddress)` | `open_tcp_listener_gated` |
| `phases_late/net_channels.rs` | `ServerSocketChannel.bind(SocketAddress,int)` | `open_tcp_listener_gated` |
| `phases_late/net_channels.rs` | `ServerSocket.bind(SocketAddress)` — channel-backed | `open_tcp_listener_gated` |
| `phases_late/net_channels.rs` | `ServerSocket.bind(SocketAddress)` — **plain** | `gate_network` before `TcpListener::bind` |
| `phases_late/net_channels.rs` | `ServerSocket.bind(SocketAddress,int)` — channel-backed | `open_tcp_listener_gated` |
| `phases_late/net_channels.rs` | `ServerSocket.bind(SocketAddress,int)` — **plain** | `gate_network` before `TcpListener::bind` |
| `net_phase_e.rs` | `re2_bind_listener` (the `java.net.ServerSocket` bind used by the RE2 socket family) | `gate_network` before `TcpListener::bind` |

### 2.3 Outbound connect and UDP (work-list item 22, swept while here)

| file | native | mechanism |
|---|---|---|
| `phases_late/net_channels.rs` | `SocketChannel.open(SocketAddress)` | `open_tcp_connect_gated` |
| `phases_late/net_channels.rs` | `SocketChannel.connect(SocketAddress)` | `open_tcp_connect_gated` |
| `phases_late/net_channels.rs` | `AsynchronousSocketChannel.connect` | `open_tcp_connect_gated` |
| `phases_late/net_channels.rs` | `WebSocket.Builder.buildAsync` (plaintext) | `open_tcp_connect_gated` |
| `phases_late/net_channels.rs` | `WebSocket.Builder.buildAsync` (TLS) | `gate_network` — `open_tls_connect` has no `_checked` twin |
| `phases_late/net_channels.rs` | `MulticastSocket.<init>()` / `<init>(I)` | `open_udp_gated` |
| `phases_early.rs` | `HttpURLConnection` connect | `open_tcp_connect_gated` |
| `net_phase_e.rs` | `DatagramSocket.<init>()` / `(I)` / `(I,InetAddress)` | `open_udp_gated` |

After this pass, **every** `FileDescriptorTable` opener call in
`native-builtins/` outside `capability_gate.rs` itself goes through a gate. The
one remaining direct opener call is `open_tls_connect`, immediately preceded by
`gate_network`.

### 2.4 M2 — raw `Unsafe` memory

`native-builtins/src/unsafe_natives.rs`, `real_ptr_read` / `real_ptr_write`.
One edit each, which is all eight raw-address `Unsafe` get/put natives
(`getByte`/`putByte`/`getShort`/`putShort`/`getInt`/`putInt`/`getLong`/`putLong`).

Both now return `Result<bool, MethodCallFailed>` instead of `bool`. `Ok(false)`
means exactly what the old `bool` meant — "not a raw pointer, or the copy
failed" — so every caller's existing fallback (the "not in any live arena"
`IllegalArgumentException`, and the stale-cache invalidation that goes with it)
is unchanged. `Err` is a capability refusal, which must not be misreported as a
use-after-free diagnosis.

The gate runs **after** the two cheap address predicates (`addr > 0`,
`!addr_is_tagged`). That ordering is deliberate twice over: an arena or tagged
address keeps its existing diagnosis untouched, and the audit report is not
flooded with rows for accesses that never dereferenced a raw pointer.

### 2.5 M3 / F2 / F3 / F4 — the FFM surface

| row | file | what changed |
|---|---|---|
| M3 (item 18) | `panama.rs`, inside `require_native_access` | one edit adds `RawMemory(op)` to all ten `MemorySegment` accessors; each passes its own `op`, so the report names the accessor |
| F2 (item 14) | `panama.rs`, `pe_downcall_invoke` | `ForeignDowncall("0x…")` beside the existing `require_native_access` — the symbol name is not carried on the handle, so the scope is the target address |
| **F3** (item 15) | `panama.rs`, `pe_upcall_handle` | added the missing `require_native_access` **and** `ForeignUpcall(target class)` |
| **F4** (item 16) | `panama.rs`, `pe_upcall_invoke` | added `ForeignUpcall(target class)` |

The upcall scope is the *class* of the Java callback (`upcall_target_name`).
That is the narrowest name reachable at those points — the stub's descriptor is
a layout list, not a method signature — and a `ForeignUpcall` denial that cannot
say which callback was refused is not actionable.

### 2.6 P3 — process spawn

`native-io/src/process.rs`, `spawn_and_wrap_with_redirects`, one line
immediately **before** `validate_spawn_program`.

The point of the row is that `validate_spawn_program` returns `Ok` for any
program when confinement is off — which is the default — so without this gate
`capability_audit(vm)` would omit `process-spawn` entirely from a run that
spawned freely. The check sits before the validator because the two answer
different questions (authority vs. sandbox geometry) and because a capability
refusal must not be translated into the `IOException` the validator's failure
becomes.

---

## 3. Sites *not* wired, and why

| row | where | why not |
|---|---|---|
| **I7** | `SecurityManager.checkRead/checkWrite/checkConnect/checkDelete` | in scope but deliberately not turned on — see §5 |
| I1 / I8 | `native-io/src/lib.rs` `validate_path` and its ~21 call sites | out of this pass's budget; needs the read/write intent threaded in, which is a signature change at every caller |
| I3 / item 9 | `native-io/src/lib.rs:1019, :1033, :1047, :1235, :10998, :11036, :11148, :11166` (`remove_dir`, `create_dir`, `create_dir_all`, `rename`) | same |
| item 24 / 25 | `native-io/src/{socket_channel,net,async_socket,datagram}.rs` | same — these already have an `outbound_policy` chokepoint to sit beside, which makes them cheap follow-ups |
| items 1–4 | `vm/`, `jit-api/` — dispatch gates and `install_capabilities` at VM init | **out of scope by instruction**; see §7 |
| items 11–13 | `lang_system.rs` exec/library-load, `phases_late.rs:1459` | in scope but not on this pass's list; `gate_process_spawn` and `gate_library_load` exist and each is a one-line edit |
| item 21 | `unsafe_natives.rs` `allocateMemory`/`freeMemory` | not on this pass's list; the arena is bounds-checked, so this is accounting rather than containment |
| item 26 | `types/src/flag_groups.rs` | **out of scope** (`types/`); see §7 |

### 3.1 A larger network surface than the audit records

While sweeping for I6 I found that `native-builtins` binds and connects
`std::net` sockets *directly* — bypassing `fd_table` entirely — in more places
than the audit's I6 row names. These are ungated and are **not** covered by this
pass beyond the three server-bind sites in §2.2:

* `servlet.rs` — `TcpListener::bind` (`:6375`), `TcpStream::connect`
  (`:2192`, `:2280`, `:6148`, `:6171`, `:6317`, `:6928`), `UdpSocket::bind` (`:3593`)
* `phases_early.rs` — `TcpListener::bind` (`:17157`, `:17186`, `:17217`, `:17249`),
  `TcpStream::connect` (`:11895`, `:16405`, `:16439`, `:16474`)
* `t27_tls.rs` — `TcpListener::bind` (`:4254`), `TcpStream::connect` (`:2981`)
* `wildfly_undertow.rs:734`, `xnio_worker.rs:1103` / `:1776` — listener bind and connect
* `net_phase_e.rs:14742` (bind), `:3680`/`:3682`/`:5846` (connect), `:12433`/`:12442` (UDP probe)
* `http2.rs:849`, `http_client.rs:322`, `http_url_connection.rs:542`/`:2109`/`:2691`,
  `net_uri_inet.rs:1721`, `inet_address.rs:432`/`:477`/`:481`, `x509_manager.rs:2558`
* `phases_late/net_channels.rs:3201` (`UdpSocket::bind`), `:5032`/`:5082`/`:5094`
  (`TcpStream::connect`)

This should become a new row in `native-capabilities.md` §1.6. It does not
change the audit's conclusion — network was already listed as ungated — but it
does change the *size* of item 22/23: they are not "switch four calls to
`_checked`", they are "give ~35 direct `std::net` call sites a `gate_network`".

---

## 4. The shape of the `Permissive` fast path

This section is a reading of the code path, not a benchmark — this pass was not
permitted to run builds, so nothing here is a measured number.

### 4.1 What a full `CapabilitySet::check` costs

`CapabilitySet::check` → `is_granted` (scan `grants`, empty by default) →
`CallSite::here()` (two compiler-provided words, free) → `record`, which is:

* a `parking_lot::Mutex` acquisition on the audit map — **shared across every
  thread of the VM**, so it is a contention point, not just an instruction cost;
* a `BTreeMap` lookup keyed on a `Capability`, whose `Ord` compares the `Scope`,
  which for `Scope::Name`/`Scope::Path` is a `String` comparison;
* and building the request itself allocates: `Capability::raw_memory(op)` →
  `Scope::name(op)` → `String`.

So: one allocation, one contended lock, one string-keyed tree lookup, per check.

### 4.2 Why that is fine almost everywhere

Every file and socket gate is immediately followed by an `open`/`connect`/`bind`
syscall, which is three to five orders of magnitude more expensive. The gate is
noise there, and those gates take the full check with exact per-call counts.

### 4.3 Why it is not fine under `Unsafe`

`real_ptr_read`/`real_ptr_write` sit under `Unsafe.getByte(long)` /
`putByte(long, byte)`, which Netty's pooled direct buffers and `java.nio.Bits`
drive **one element at a time** — the module's own header comment describes a
tight `for (i…) unsafe.putByte(addr + i, b)` loop as the motivating case for the
existing thread-local arena cache. A malloc plus a contended mutex per byte is
not affordable.

`gate_raw_memory` therefore memoizes the resolved verdict per `(thread, vm)` in
a `thread_local! { Cell<Option<(usize, RawGate)>> }` — a `Copy` payload, so no
`RefCell` borrow flag, no `Arc` clone, no allocation:

| policy | first call on the thread | every later call |
|---|---|---|
| **none installed (today's default)** | one `capabilities_for` lookup (global mutex + scan of a ≤3-element `Vec`) | TLS load, `usize` compare, discriminant compare |
| `Permissive` | one full `check` — records the capability, its scope and its first site | TLS load, `usize` compare, discriminant compare |
| `Audit` / `Enforce` | full `check` | full `check` (no memo — the per-call tally and the refusal *are* the point) |

The steady-state permissive path is what the brief asked for: a thread-local
load and an already-resolved discriminant test, with no allocation, no lock and
no atomic. It is preceded by `ctx.vm_identity()`, which is one virtual call
returning a field.

### 4.4 The fidelity this trades away, stated plainly

Under `Permissive`, `RawMemory` is recorded **once per thread**, not once per
access. The audit report names the capability, its scope and its first call site
correctly, and **under-reports `count`**. `count` is the one number the report's
stated purpose — deriving a least-privilege grant set — does not depend on.
`Audit` mode, which exists precisely to price an `Enforce` flip, takes the
unmemoized arm and counts every call exactly. Two unit tests pin both halves of
this:
`permissive_raw_memory_records_the_capability_then_goes_transparent` and
`audit_counts_every_raw_memory_access_not_just_the_first`.

The FFM `MemorySegment` accessors deliberately use the *unmemoized*
`gate_raw_memory_named` instead: they already take a policy read-lock and a
`SecurityManager` check per call, so the full check is noise there, and each
accessor must report its own name — which a single-slot memo could not preserve.

### 4.5 Staleness, and the out-of-scope fix that removes it

The memo assumes a VM installs its policy during init, before Java code runs —
which is exactly what work-list item 3 specifies. A policy installed *after* a
thread has already taken a raw-memory path is not seen by that thread until
`capability_gate::reset_raw_memory_gate_memo()` is called.

**The clean fix is one edit in `native-api`, which this pass could not make:**
add a process-wide `AtomicUsize` count of installed sets to
`capability.rs`, bumped by `install_capabilities` and decremented by
`uninstall_capabilities`, exposed as

```rust
/// Whether any VM in this process has a capability policy installed.
/// A relaxed load — cheap enough to put in front of a per-byte gate.
pub fn any_capabilities_installed() -> bool;
```

`gate_raw_memory` would then be a single relaxed atomic load with **no memo and
no staleness at all**, and the `count` fidelity note in §4.4 could be dropped
because the full check would only ever run when a policy actually exists. This
is the single highest-value follow-up in this document.

---

## 5. I7 — the `SecurityManager` verdict

**The call sites belong in `native-builtins` and `native-io`, not in `vm/`.**

`SecurityManager.checkRead/checkWrite/checkConnect/checkDelete` are registered
and functional at `native-builtins/src/security_manager.rs:779`, `:791`,
`:801`, `:821`. `checkExec` is the only one with a caller, and that caller is
`check_exec_or_throw` at `native-builtins/src/lang_system.rs:1843` — a
`native-builtins` function, invoked from `native-builtins` and `native-io`
natives. `vm/` performs no file or socket syscalls of its own; every one goes
through a native in one of these two crates. So a sibling
`check_read_or_throw` / `check_write_or_throw` / `check_connect_or_throw` /
`check_delete_or_throw`, written exactly like `check_exec_or_throw` (read the
singleton, return `Ok(())` when absent, otherwise
`ctx.invoke_virtual(sm_ref, "checkRead", "(Ljava/lang/String;)V", …)`), would
sit next to every gate this pass added.

**They are deliberately not wired.** Unlike a capability check, a
`SecurityManager` callback is *not* permissive by default: it is inert only
while no manager is installed, and it becomes live the moment an application
calls `System.setSecurityManager`. Turning these on is therefore a live
behaviour change for every workload that installs a manager today and currently
has its file and socket access silently unchecked — the exact thing this pass
was told not to do. It also inherits all four defects `native-capabilities.md`
§7.1 documents about the singleton (cross-VM interference, privilege escalation
by removal, and a use-after-move under a moving young GC).

The exact list, if the decision is made to wire them, is the set of gates in §2
plus the `native-io` rows in §3:

| callback | call sites |
|---|---|
| `checkRead` | every `open_read_gated` / `open_read_write_gated` site in §2.1; `native-io/src/lib.rs:1404`, `:7814`, `:11387`, `:11415`; `native-io/src/random_access_file.rs:277` |
| `checkWrite` | every `open_write_gated` / `open_read_write_gated` site in §2.1; `native-io/src/lib.rs:1803`, `:1823`, `:1847`, `:1872`, `:11387`, `:11415` |
| `checkDelete` | `native-io/src/lib.rs:1019`, `:1235`, `:10998`, `:11166` (`remove_dir`, `rename`) plus the `File.delete` natives the audit's I3 row lists |
| `checkConnect` | every `open_tcp_connect_gated` / `gate_network` connect site in §2.3, plus the ~20 direct `TcpStream::connect` sites in §3.1 |

**Recommendation:** do not wire them. `CapabilitySet` is the replacement for
this API (`SecurityManager` is terminally deprecated under JEP 411/486, which
`native-capabilities.md` §1 already gives as a reason the capability model
exists), it is per-VM rather than process-global, and it now covers the same
sites. Wiring a deprecated process-global gate to the same call sites a
per-VM gate already covers adds a second policy that can disagree with the
first.

---

## 6. The one behaviour change

`native-builtins/src/panama.rs`, `pe_upcall_handle`: the added
`require_native_access(ctx, "upcallHandle")?`.

This is gap **F3** and it is the only edit in this pass that can refuse an
operation in the default configuration. Before it, `Linker.upcallHandle` had no
native-access gate at all while `pe_downcall_invoke` failed closed — even though
`docs/CONFIG.md:216` has always documented `Linker.upcallHandle` as consulting
the access registry, and even though the upcall is the *more* dangerous
direction: it hands native code a real extern "C" trampoline into Java.

With `--enable-native-access` absent, `upcallHandle` now throws
`IllegalCallerException` instead of succeeding. One unit test needed the
existing `NativeAccessGuard::enable()` added
(`panama::tests::new18_upcall_libffi_closure_dispatches_to_java`); nothing else
in the tree calls it.

`pe_upcall_invoke` (**F4**) got only the capability check, not
`require_native_access` — the work list asks for the native-access gate at F3
only, and gating the Java-side dispatch as well would double the change.

If a workload regresses on this, **delete that one line**. The `ForeignUpcall`
capability check beside it is behaviour-neutral on its own and should stay.

One cosmetic wart: `require_native_access` formats its message as
`"Native access is not enabled for this module (MemorySegment.{op} denied)"`, so
the upcall denial reads `MemorySegment.upcallHandle`. The message text is
asserted in one test (`panama.rs`, via `message.contains("Native access is not
enabled")`) which is unaffected, but the `MemorySegment.` prefix should be made
conditional in a follow-up.

---

## 7. Edits required outside this pass's scope

| # | file:line | edit | why it matters |
|---|---|---|---|
| 1 | `native-api/src/capability.rs` | add an `AtomicUsize` install count and `pub fn any_capabilities_installed() -> bool` (§4.5) | removes the raw-memory memo, its staleness window, and the `count` fidelity caveat |
| 2 | `vm/src/vm/vm_init.rs:3099` and the `SharedVm` construction that sets `vm_identity` | build `CapabilitySet::from_env(VmId::from_raw(vm_identity))`, `registry.set_capabilities(arc.clone())` before the `register_*` pass, `install_capabilities(arc)` | **nothing in this document does anything until this lands.** Every gate here resolves `None` today |
| 3 | VM teardown, paired with #2 | `uninstall_capabilities(vm)` | a long-lived host process otherwise accumulates dead entries |
| 4 | `vm/src/vm/vm_exec.rs:13896`, `:21745`, `:21866` | call `registry.check_dispatch_capability(id)` after `find_with_kind` | the registry-side safety net under the per-site gates |
| 5 | `jit-api/src/lib.rs:495` | same, beside `record_invocation(id)` | otherwise a JIT-dispatched native skips the net |
| 6 | `types/src/flag_groups.rs` (`SCALARS`) | declare `CRATONVM_CAPABILITY_MODE`, `CRATONVM_CAPABILITY_GRANTS`, `CRATONVM_CAPABILITY_LOG` | undeclared names are served by live `std::env` reads instead of the frozen `VmFlags` snapshot, so `set_var` is invisible to tests and `-XX:` options cannot reach them |

---

## 8. Tests

`native-builtins/src/capability_gate.rs`, `mod tests`:

* **no policy** — the helpers *are* the raw openers, the raw-memory gate is a
  no-op, and `capability_audit(vm)` is `None`;
* **`Permissive` is observably identical** — the same read through the raw
  opener and through the gate returns the same length and the same bytes, and
  the use is still recorded;
* **`#[track_caller]` survives the helper** — the recorded `first_site` is the
  caller, not `capability_gate`'s internals;
* **`Audit` allows but tallies** — `total_ungranted() >= 1` with no grants;
* **`Audit` counts every raw-memory access**, `Permissive` counts one per
  thread (both halves of the §4.4 trade);
* **`Enforce` denies** an ungranted read, and the refusal is
  `FdCapabilityError::Denied`, not an I/O error;
* **the refusal precedes the syscall** — for a path that does not exist, the raw
  opener answers `NotFound` and the gated opener answers `Denied`, which is only
  possible if the check ran before `fs::File::open`; so no fd was reserved;
* **a denied write creates no file**;
* **a granted prefix admits what is under it and refuses a traversal out of it**;
* **read-write open needs both capabilities**;
* **a bind outside the granted port range is refused and still recorded**;
* **`Enforce` keeps denying raw memory** (the memo does not cache a verdict
  away) and the denial maps to `SecurityException`;
* **the derived grant set round-trips** — `suggested_grants()` from an `Audit`
  run, loaded into a fresh `Enforce` set, admits what the run did and refuses
  what it did not.

Each test runs under its own `VmId`, handed out by `ctx_with_private_vm()`.
This matters: `install_capabilities` is a process-global index keyed on
`vm_identity()`, every other mock context in the crate reports the default `0`,
and an `Enforce` policy installed under `0` would be visible to every test
running in parallel and could refuse *their* I/O. `MockNativeContext` gained a
settable `vm_identity_override` (default `0`, so every existing test is
unaffected) for exactly this.

The "a denied open reserves no descriptor" invariant is asserted directly on
`FileDescriptorTable` in `native-api/src/fd_table.rs`'s own tests
(`native-capabilities.md` §9); the mock context shares one process-wide fd
table, so an fd-counter assertion here would be racy against the rest of the
suite. The ordering test above is the crate-local proof that the gate runs
first.

---

## 9. Related

* `docs/security/native-capabilities.md` — the audit inventory, the capability
  model, the three modes, and the ordered plan to default-deny.
* `docs/SECURITY_HARDENING.md` — the `CRATONVM_CONFINE_IO` /
  `CRATONVM_UNTRUSTED_CODE` profiles this does not replace.
* `docs/CONFIG.md` §`--enable-native-access` — the FFM access registry §6 makes
  `Linker.upcallHandle` actually consult.
