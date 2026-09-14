# Native and foreign capability gating

**Status:** mechanism landed, **enforcement off by default**.
**Crate:** `native-api` (`native-api/src/capability.rs`).
**Addresses:** C2 review P0 *"Gate all native and FFI capabilities"* (confidence:
Confirmed) and the related finding that `System.setSecurityManager` installs a
process-wide singleton — **that second finding is now fixed** (per-VM index +
GC root source; see §7.1). The capability gate itself is still the mechanism,
not the enforcement.

> **Read this first.** The capability gate ships in `Permissive` mode. It allows
> everything and records what was used. Nothing in this document is enforced
> unless an operator sets `CRATONVM_CAPABILITY_MODE=enforce`. §6 is the ordered
> plan for changing that; §7 is the honest statement of what an untrusted class
> can still reach today.

---

## 1. Audit inventory

Every row is a place where Java-reachable code causes a host-level effect. The
"check today" column records what actually runs at that site, not what the class
name suggests.

### 1.1 Registration and dispatch (`native-api`)

| # | Entry point | file:line | Check today | Reachable with no policy consultation? | Scope of the check |
|---|---|---|---|---|---|
| R1 | `NativeMethodRegistry::register` | `native-api/src/registry.rs:4760` | `CompatibilityMode::JdkOnly` refusal, `CRATONVM_NO_STUBS`, `CRATONVM_REAL_NET_SOCKETS`, real-JDK layout drops — all **compatibility** filters, none security | **Yes** (before this change) | per-VM (registry field) |
| R2 | `NativeMethodRegistry::find` / `find_with_kind` / `callback_of` | `native-api/src/registry.rs:5801`, `:5525`, `:5628` | none | **Yes** | n/a |
| R3 | Interpreter native dispatch | `vm/src/vm/vm_exec.rs:13896`, `:21745`, `:21866` | `NativeKind::allowed_in(compatibility_mode)` only | **Yes** | per-VM |
| R4 | JIT native dispatch | `jit-api/src/lib.rs:480-496` | `record_invocation` census only | **Yes** | per-VM |
| R5 | `NativeContext::load_native_library` / `find_native_symbol` / `unload_native_library` | `native-api/src/registry.rs:3859`, `:3866`, `:3891` | none at the trait boundary | **Yes** | per-VM (VM impl) |
| R6 | `FileDescriptorTable::open_read` / `open_write` / `open_read_write` / `open_random_access` | `native-api/src/fd_table.rs:407`, `:434`, `:830`, `:865` | **none** — the doc comment explicitly says the path is passed "verbatim to the OS with no path-traversal or sandbox check" and delegates that duty to the caller | **Yes** | n/a (table owns no VM id) |
| R7 | `FileDescriptorTable::open_tcp_connect` / `open_tcp_listener` / `open_udp` / `open_tls_connect` | `native-api/src/fd_table.rs:1301`, `:1376`, `:1132`, (tls) | none | **Yes** | n/a |
| R8 | `NativeMemoryTable::allocate` / `get_ptr` / `free` | `native-api/src/ffi.rs:129`, `:238`, `:205` | bounds + generation handles (M4b/M4c) — a **memory-safety** check, not an authority check | **Yes** | per-VM (table instance) |
| R9 | `UpcallTable::register` / `get` / `get_checked` | `native-api/src/ffi.rs:501`, `:536`, `:544` | generation handles (M4c) | **Yes** | per-VM |

### 1.2 Process spawn

| # | Entry point | file:line | Check today | Reachable with no policy consultation? | Scope |
|---|---|---|---|---|---|
| P1 | `Runtime.exec*` → `runtime_spawn_process` | `native-builtins/src/lang_system.rs:1903` | `check_exec_or_throw` → `SecurityManager.checkExec` | Yes when **no SM installed** (the default) — `check_exec_or_throw` returns `Ok(())` immediately (`lang_system.rs:1850`) | **process-global** (`SECURITY_MANAGER`) |
| P2 | `ProcessBuilder.start` (phases_late) | `native-builtins/src/phases_late.rs:1459` | same | same | process-global |
| P3 | `ProcessBuilder.start` (native-io real spawn) | `native-io/src/process.rs:443` → `validate_spawn_program` | CWD-confinement check, **opt-in and off by default** (`native-io/src/process.rs:338`: "Default (unconfined) profile: JDK-faithful, spawn freely") | **Yes** by default | **process-global** (`PATH_CONFINE_TO_CWD`) |
| P4 | Panama downcall to `execve`/`CreateProcessW` | `native-builtins/src/panama.rs:2323` | `require_native_access` only — never reaches `checkExec`. This bypass is called out in a standing `Audit TODO (Panama)` at `lang_system.rs:1837` | Yes, whenever native access is granted | process-global |

### 1.3 Native library loading

| # | Entry point | file:line | Check today | Reachable with no policy consultation? | Scope |
|---|---|---|---|---|---|
| L1 | `Runtime.loadLibrary0` / `Runtime.load0` | `native-builtins/src/lang_system.rs:1323`, `:1339` | `check_host_native_access_or_throw` | Denies under `CRATONVM_UNTRUSTED_CODE`; **otherwise returns `Ok` when no SM is installed** (`security_manager.rs:83`) | **process-global** |
| L2 | `System.loadLibrary` / `System.load` | `native-builtins/src/lang_system.rs:1353`, `:1367` | same | same | process-global |
| L3 | `SymbolLookup.libraryLookup` | `native-builtins/src/panama.rs:1841`, `:1957` | `require_native_access` (secure-by-default `None`) | No — fails closed | **process-global** (`NATIVE_ACCESS_POLICY`) |

### 1.4 Raw memory

| # | Entry point | file:line | Check today | Reachable with no policy consultation? | Scope |
|---|---|---|---|---|---|
| M1 | `Unsafe.allocateMemory` / `reallocateMemory` / `freeMemory` | `native-builtins/src/unsafe_natives.rs:971+` | consolidated bounds-checked arena store (V5) | **Yes** — memory-safe, not authority-gated | per-VM arena |
| M2 | `Unsafe.getLong(J)` / `putLong(JJ)` / `getByte` / `putByte` | `native-builtins/src/unsafe_natives.rs:1770`, `:1793`; impl `:547` | arena tag check, then **`real_ptr_read`/`real_ptr_write` fallback for any untagged address** (`unsafe_natives.rs:187-193`) | **Yes** | none |
| M3 | `MemorySegment.get/set/getAtIndex/setAtIndex/copy/fill` | `native-builtins/src/panama.rs:776`, `:799`, `:1175`, `:1226`, `:1340`, `:1622`, `:1635`, `:1650`, `:1702` | `require_native_access` | No — fails closed | **process-global** |
| M4 | `MemorySegment.ofAddress(J)` | `native-builtins/src/panama.rs:898` | `native_access_enabled()`, with a documented `addr == 0` bootstrap exemption | No (except address 0) | process-global |
| M5 | `MemorySegment.reinterpret(J)` | `native-builtins/src/panama.rs:3979` | `native_access_enabled()` | No | process-global |
| M6 | Direct `ByteBuffer` backing store | `native-io/src/direct_buffer.rs` | bounds only | **Yes** | n/a |

### 1.5 Foreign downcall / upcall

| # | Entry point | file:line | Check today | Reachable with no policy consultation? | Scope |
|---|---|---|---|---|---|
| F1 | `Linker.downcallHandle(...)` (handle creation) | `native-builtins/src/panama.rs:2091`, `:2106` | **none** | **Yes** (invocation is gated, creation is not) | — |
| F2 | `pe_downcall_invoke` (the actual `libffi` call) | `native-builtins/src/panama.rs:2323` | `require_native_access` + `validated_fn_ptr` null/alignment check | No — fails closed | **process-global** |
| F3 | `Linker.upcallHandle(...)` | `native-builtins/src/panama.rs:3184` (`pe_upcall_handle`) | **none — no `require_native_access` call in this function** | **Yes** | — |
| F4 | `pe_upcall_invoke` | `native-builtins/src/panama.rs:3271` | none | **Yes** | — |
| F5 | `validated_fn_ptr` | `native-builtins/src/panama.rs:238` | `native_access_enabled()`, null, alignment | No | process-global |

### 1.6 File and network I/O

| # | Entry point | file:line | Check today | Reachable with no policy consultation? | Scope |
|---|---|---|---|---|---|
| I1 | `native-io` file natives | `native-io/src/lib.rs:379` (`validate_path`) | NUL rejection + escaping-`..` guard always on; **CWD confinement + sandbox roots opt-in, off by default** (`native-io/src/lib.rs:299`) | Partly — traversal is blocked, location is not | **process-global** (`PATH_CONFINE_TO_CWD`, `SANDBOX_ROOTS`) |
| I2 | `java.nio.file` natives in `native-builtins` — `Files.newInputStream`, `Files.newBufferedWriter`, `FileChannel.open`, … | `native-builtins/src/phases_late/nio_file.rs:5483`, `:5486`, `:5912`, `:7578`, `:7673`, `:9523`, `:9531` | **none.** These call `ctx.fd_table().open_read/open_write/open_read_write` directly. `grep -c validate_path native-builtins/src` is **0** | **Yes — this is the largest single gap** | n/a |
| I3 | `File.delete` / `mkdir` / `renameTo` / `createNewFile` | `native-io/src/lib.rs:1019`, `:1033`, `:1047`, `:1235`, `:10998`, `:11036`, `:11148`, `:11166` | `validate_path` (see I1) | Location not restricted by default | process-global |
| I4 | Outbound TCP connect | `native-io/src/net.rs:1305`, `native-io/src/socket_channel.rs:1741`, `:1761`, `:1875`, `native-io/src/async_socket.rs:972` | `outbound_policy::policy_connect` — default rejects only the cloud-metadata link-local addresses | Everything else: **yes** | **process-global** (`native-io/src/outbound_policy.rs`) |
| I5 | UDP send | `native-io/src/datagram.rs:239` | `check_outbound` | same | process-global |
| I6 | Server socket bind / accept | `native-builtins/src/phases_late/net_channels.rs:264`, `:317`, `:4710`, `:4763` | **none** | **Yes** | n/a |
| I7 | `SecurityManager.checkRead` / `checkWrite` / `checkConnect` / `checkDelete` | `native-builtins/src/security_manager.rs:779`, `:791`, `:801`, `:821` | registered natives that work correctly — but **no file or socket path in the VM calls them.** `checkExec` is the only one with a caller | — | process-global |

### 1.7 Summary of the inventory

* Of the 32 rows above, **21 are reachable today with no policy consultation of
  any kind** under the default configuration.
* Every check that does exist is **process-global**: `SECURITY_MANAGER`
  (`security_manager.rs:117`), `ACTIVE_POLICY_OBJECT` (`:205`),
  `NATIVE_ACCESS_POLICY` (`panama.rs:99`), `PATH_CONFINE_TO_CWD`
  (`native-io/src/lib.rs:299`), `SANDBOX_ROOTS` (`:149`), the outbound policy
  hook and connect timeout (`outbound_policy.rs`).
* The only checks that are already **per-VM** are the ones that are not security
  checks: `NativeMethodRegistry::compatibility_mode`, the `NativeMemoryTable`
  and `UpcallTable` instances, and the `Unsafe` arena store.
* The FFM gate (`NATIVE_ACCESS_POLICY`) is the one mechanism that is
  secure-by-default — but its own doc comment records that it answers "is native
  access granted to **any** module?" rather than the per-caller question,
  because the calling module is not reachable at the gate point
  (`panama.rs:47-64`).

---

## 2. The capability model

`native-api/src/capability.rs`.

```
Capability = FileRead(Scope) | FileWrite(Scope) | Network(Scope)
           | ProcessSpawn(Scope) | LibraryLoad(Scope) | RawMemory(Scope)
           | ForeignDowncall(Scope) | ForeignUpcall(Scope) | NativeRegister(Scope)

Scope      = Any
           | Path(String)                      // normalized
           | Endpoint { host, port: PortSpec }
           | Name(String)

PortSpec   = Any | Exact(u16) | Range(u16, u16)
```

**Every variant carries a scope**, including the ones the exit criterion spells
without one. A `ProcessSpawn` denial that cannot say *which program* was refused
is not actionable, and `CapabilityDenied` is required to report the scope. A
gate with nothing narrower to say passes `Scope::Any`, which behaves exactly
like an unscoped variant.

### 2.1 Scope matching is grant-admits-request, and is not symmetric

* `Scope::Any` as a **grant** is a wildcard; as a **request** it means "the gate
  could not name what it is touching", and only an `Any` grant admits it. This
  is the fail-closed direction: an unparseable address or an unnamed operation
  can never slide under a narrow grant.
* `Path` grants are containment claims. Both sides are normalized first
  (`normalize_path`): `\` folds to `/`, `.` is dropped, interior `..` is
  resolved, a `..` at depth zero is *kept* on a relative path (marking a genuine
  escape) and *clamped* on an absolute one, and on Windows the result is
  lower-cased because NTFS is case-insensitive and a case-sensitive prefix test
  would be a bypass. So `/data/../etc/passwd` becomes `/etc/passwd` and cannot
  satisfy a `/data` grant, while `/data/sub/../file` becomes `/data/file` and
  can — matching the JDK's own behaviour, which `native-io`'s
  `has_escaping_parent_segment` documents at `native-io/src/lib.rs:240-258`.
* A shared textual prefix is not containment: `/data` does not admit
  `/database/file`.
* `Endpoint` grants match host by `*`, `*.suffix` (the suffix domain and
  everything under it), or exact, all case-insensitively; port by `Any`,
  `Exact`, or an inclusive `Range`.
* `Name` grants match `*`, `prefix*`, or exact. A `Name` grant may cover a
  `Path` request (so `library-load:libssl*` covers both
  `System.loadLibrary("ssl")` and an absolute `System.load` path); the reverse is
  refused, because a path grant is a containment claim and must not be satisfied
  by a bare name.
* Scopes of different shapes never admit each other in any other combination.

### 2.2 Per-VM ownership

```rust
let vm  = VmId::of(ctx);                       // or VmId::from_raw(vm_identity)
let mut set = CapabilitySet::from_env(vm);     // mode + grants from the environment
set.grant(Capability::parse_grant("file-read:/data").unwrap());
let set = Arc::new(set);
registry.set_capabilities(Arc::clone(&set));   // registration + dispatch gate
install_capabilities(set);                     // per-call-site gates via NativeContext
```

The API shape is what enforces the per-VM property:

* `CapabilitySet::new` and `from_env` **require** a `VmId`; there is no `Default`
  impl.
* There is no `CapabilitySet::current()`, no `check(cap)` free function, and no
  "the" policy. `capabilities_for`, `capability_audit` and
  `uninstall_capabilities` all take a `VmId`.
* `VmId` is a newtype over `NativeContext::vm_identity()`, so a `ClassId`, an
  fd, or a `0`-means-global sentinel cannot be passed by accident, and
  `grep VmId` finds every scoped decision.
* `NativeMethodRegistry::capabilities` is a **field**, for the same reason
  `compatibility_mode` is one (`registry.rs:4364-4384`, and the standing memory
  note *process-global native caches must be VM-scoped*).
* `capability::VM_CAPABILITIES` is a process-global **index**, not a
  process-global **policy**: nothing can be read out of it without a `VmId`, and
  two VMs get two entries. The failure being fixed is one policy shared by two
  VMs, not one lookup table holding two policies.

### 2.3 Denial

```rust
pub struct CapabilityDenied {
    pub capability: CapabilityKind,   // what kind was refused
    pub scope: Scope,                 // the concrete thing that was refused
    pub vm: VmId,                     // which VM's policy refused it
    pub site: CallSite,               // file:line of the gate (#[track_caller])
}
```

`Display` renders all four plus a paste-able grant line, and
`suggested_grant()` returns just the grant. `From<CapabilityDenied>` exists for
`RuntimeError::SecurityException` and `MethodCallFailed`, so a gate is
`ctx.check_capability_or_throw(cap)?`.

The call site is captured with `#[track_caller]`, the same technique
`NativeMethodRegistry::register` uses for registration provenance
(`registry.rs:4751-4759`): two compiler-provided words, no allocation, no
`format!` until something renders the message.

---

## 3. The three modes

| Mode | Allows | Records | Use |
|---|---|---|---|
| `Permissive` (**default**) | everything | each distinct `(kind, scope)` with a count and first call site; logs first use when `CRATONVM_CAPABILITY_LOG` is set | today's behaviour, plus visibility |
| `Audit` | everything | the same, **plus** a per-entry `ungranted` tally of what an `Enforce` flip would have refused | the mode to run a suite in before flipping |
| `Enforce` | only what a grant admits | the same, with `ungranted` counting actual refusals | a hardened deployment |

Configuration, following the crate's existing env idiom (`native_ring.rs:281`,
`registry.rs:4507`), read through `cratonvm_types::flags::runtime_var`:

```
CRATONVM_CAPABILITY_MODE=permissive|audit|enforce      # default: permissive
CRATONVM_CAPABILITY_GRANTS='file-read:/data;file-write:/var/app;network:*.example.com:443;library-load:libssl*'
CRATONVM_CAPABILITY_LOG=1                              # log first use of each capability
```

The mode is deliberately **not** memoized in a `OnceLock`. It is read once per
VM construction; latching it process-wide would reintroduce exactly the cross-VM
coupling this module exists to remove.

### 3.1 The audit report

```rust
let report = capability_audit(vm).unwrap();
println!("{report}");                       // human-readable table
println!("{}", report.suggested_grants());  // paste into CRATONVM_CAPABILITY_GRANTS
```

`suggested_grants()` emits one grant per distinct scope actually exercised, so
the output is exact rather than generalized — widening `/data/a` and `/data/b`
into a shared `/data` prefix is a judgement call left to the operator. The unit
test `audit_report_is_sorted_deterministic_and_derives_a_working_grant_set`
asserts the round trip: the derived grant list, loaded into a fresh `Enforce`
set, admits every capability the run exercised and nothing else.

Report rows are ordered deterministically (kind in declaration order, then
scope) because the audit log is a `BTreeMap`, so two runs of the same workload
produce diffable output. The map is capped at `MAX_AUDIT_ENTRIES` (4096) so a
workload that opens a million distinct temp files cannot grow it without bound;
past the cap, counters for known entries keep moving, new entries are dropped,
and `CapabilityAuditReport::truncated` is set — the report never silently claims
to be complete.

---

## 4. What is wired today

### 4.1 Inside `native-api` (done)

| Chokepoint | Capability | Where |
|---|---|---|
| `NativeMethodRegistry::register` | `NativeRegister(class.method)` | `registry.rs`, last drop arm, immediately before the digest is computed. A refusal returns without inserting — nothing reaches `registrations`/`categories`/`provenance`/`slots`, the triple never appears in the census, and `generation()` does not move (same contract as the JDK-only refusal) |
| `NativeMethodRegistry::check_dispatch_capability(id)` | the kind from `classify_native`, `Scope::Any` | `registry.rs`. Classification is computed once per slot at registration time into a sparse `slot -> kind` map, so dispatch is an integer lookup, not a string match. **Not yet called by the interpreter or JIT** — see §5 |
| `FileDescriptorTable::open_read_checked` | `FileRead(path)` | `fd_table.rs` |
| `…::open_write_checked` | `FileWrite(path)` | `fd_table.rs` |
| `…::open_read_write_checked` | `FileRead` **and** `FileWrite` | `fd_table.rs` — the fd can do either, so one grant is not enough |
| `…::open_random_access_checked` | `FileRead`, plus `FileWrite` when `write` | `fd_table.rs` |
| `…::open_tcp_connect_checked` | `Network(host:port)` | `fd_table.rs` |
| `…::open_tcp_listener_checked` | `Network(bind host:port)` | `fd_table.rs` — binding is its own authority, not a weaker form of connecting |
| `…::open_udp_checked` | `Network(bind)` or `Network(Any)` when unbound | `fd_table.rs` |

Every checked opener runs the check **before the fd is reserved and before the
syscall**, so a denial has no observable effect: no descriptor is consumed, no
file is created, no connection is attempted. Two unit tests assert exactly that.

`FdCapabilityError` distinguishes `Denied` from `Io` because callers must
translate them differently — a denial is a `SecurityException` (refused before
the syscall, must not be retried), an I/O failure is the `IOException` the JDK
method already documents. `From<FdCapabilityError> for io::Error` exists as a
lossy fallback (`PermissionDenied`) for the many call sites that can only
produce an `io::Error` today.

### 4.2 Public API added for out-of-crate gates

```rust
// The one-liner for any native holding a &dyn NativeContext:
use cratonvm_native_api::{Capability, CapabilityCheck};
ctx.check_capability_or_throw(Capability::file_read(&path))?;

// For a gate on a genuinely hot path, hold the Arc instead of re-resolving:
if let Some(caps) = ctx.vm_capabilities() {
    caps.check_or_throw(Capability::network(&addr))?;
}
```

`CapabilityCheck` is blanket-implemented for every `NativeContext` (including
`dyn NativeContext`), exactly like `ClassDiscriminator` — so no implementor has
to change. With no policy installed it is a lock, a scan of a three-element
`Vec`, and `Ok(())`.

---

## 5. Ordered out-of-crate work list

Each row is a single edit. None of them changes behaviour while the mode is
`Permissive`. Ordered by *coverage per edit*.

| # | File:line | Capability | Sits beside |
|---|---|---|---|
| 1 | `vm/src/vm/vm_exec.rs:13896` (and `:21745`, `:21866`) — after `find_with_kind`, before invoking the callback | call `registry.check_dispatch_capability(id)` | the existing `NativeKind::allowed_in(compatibility_mode)` test |
| 2 | `jit-api/src/lib.rs:495` — beside `registry.record_invocation(id)` | same | `record_invocation` |
| 3 | `vm/src/vm/vm_init.rs:3099` (where `fd_table: FileDescriptorTable::new()` is built) and the `SharedVm` construction that sets `vm_identity` | build `CapabilitySet::from_env(VmId::from_raw(vm_identity))`, then `registry.set_capabilities(arc.clone())` **before** the `register_*` population pass, and `install_capabilities(arc)` | `NativeMethodRegistry::set_compatibility_mode` |
| 4 | VM teardown (paired with #3) | `uninstall_capabilities(vm)` | wherever `vm_identity` is retired |
| 5 | `native-builtins/src/phases_late/nio_file.rs:7578`, `:9523` | `FileRead(path)` — switch to `open_read_checked` | nothing exists today (**gap I2**) |
| 6 | `native-builtins/src/phases_late/nio_file.rs:5912`, `:7673` | `FileWrite(path)` — switch to `open_write_checked` | nothing exists today |
| 7 | `native-builtins/src/phases_late/nio_file.rs:5483`, `:5486`, `:9531` | `FileRead` + `FileWrite` — switch to `open_read_write_checked` | nothing exists today |
| 8 | `native-io/src/lib.rs:379` (`validate_path`) | `FileRead`/`FileWrite` depending on the caller — thread the mode in, or gate at each of the 21 call sites | the NUL and escaping-`..` checks |
| 9 | `native-io/src/lib.rs:1019`, `:1021`, `:1033`, `:1047`, `:1235`, `:10998`, `:11000`, `:11036`, `:11051`, `:11148`, `:11154`, `:11166` | `FileWrite(path)` (both source *and* destination for rename/copy) | the `validate_path` call already present |
| 10 | `native-io/src/process.rs:443` | `ProcessSpawn(program)` | `validate_spawn_program(program)` — same line, before it |
| 11 | `native-builtins/src/lang_system.rs:1903` and `:2522` | `ProcessSpawn(program)` | `check_exec_or_throw(ctx, program)?` — immediately before |
| 12 | `native-builtins/src/phases_late.rs:1459` | `ProcessSpawn(cmd_strings[0])` | `check_exec_or_throw` |
| 13 | `native-builtins/src/lang_system.rs:1323`, `:1339`, `:1353`, `:1367` | `LibraryLoad(name)` | `check_host_native_access_or_throw(ctx, &name)?` — immediately before |
| 14 | `native-builtins/src/panama.rs:2327` | `ForeignDowncall(symbol_or_address)` | `require_native_access(ctx, "downcall")?` |
| 15 | `native-builtins/src/panama.rs:3184` (`pe_upcall_handle`) | `ForeignUpcall(target)` | **nothing — this function has no native-access gate at all** (gap F3); add `require_native_access` here too |
| 16 | `native-builtins/src/panama.rs:3271` (`pe_upcall_invoke`) | `ForeignUpcall` | nothing (gap F4) |
| 17 | `native-builtins/src/panama.rs:1841`, `:1957` | `LibraryLoad(path)` | `require_native_access(ctx, "libraryLookup")?` |
| 18 | `native-builtins/src/panama.rs:221` (`require_native_access`) | `RawMemory(op)` — one edit covers all 10 `MemorySegment` accessors | inside the helper, after the `native_access_enabled()` test |
| 19 | `native-builtins/src/panama.rs:898`, `:3979` | `RawMemory("ofAddress")` / `RawMemory("reinterpret")` | the inline `native_access_enabled()` tests |
| 20 | `native-builtins/src/unsafe_natives.rs:187`, `:193` (`real_ptr_read`/`real_ptr_write`) | `RawMemory("Unsafe.rawAddress")` — one edit covers every untagged-address `Unsafe` get/put | inside the helper (gap M2) |
| 21 | `native-builtins/src/unsafe_natives.rs:971+` (`allocateMemory`, `freeMemory`, …) | `RawMemory("Unsafe.allocateMemory")` | the arena-store routing |
| 22 | `native-builtins/src/phases_late/net_channels.rs:46`, `:80`, `:1465`, `:2027`, `:2030`; `native-builtins/src/phases_early.rs:19990` | `Network(addr)` — switch to `open_tcp_connect_checked` | nothing exists today |
| 23 | `native-builtins/src/phases_late/net_channels.rs:264`, `:317`, `:4710`, `:4763` | `Network(bind addr)` — switch to `open_tcp_listener_checked` | nothing exists today (**gap I6**) |
| 24 | `native-io/src/socket_channel.rs:1741`, `:1761`, `:1875`; `native-io/src/net.rs:1305`; `native-io/src/async_socket.rs:972` | `Network(target)` | `outbound_policy::check_outbound` / `policy_connect` — immediately before |
| 25 | `native-io/src/datagram.rs:239` | `Network(target)` | `check_outbound(&literal)` |
| 26 | `types/src/flag_groups.rs` (`SCALARS`) | declare `CRATONVM_CAPABILITY_MODE`, `CRATONVM_CAPABILITY_GRANTS`, `CRATONVM_CAPABILITY_LOG` | the other declared scalars |

Item 26 matters for a reason specific to this repo: undeclared names are served
by live `std::env` reads rather than the frozen `VmFlags` snapshot (see the
standing note *declared flags latch, so `set_var` is invisible to tests*).
Declaring them makes the capability configuration behave like every other flag —
and makes it settable through `-XX:` options via `flags::install`. Until then,
`CRATONVM_CAPABILITY_MODE` is read live, which is correct but inconsistent.

---

## 6. The ordered plan to default-deny, and what breaks first

Flipping the default to `Enforce` today would break **everything**, in this
order. This is not a hedge: an empty grant set denies every capability, and the
first failure would be during registration.

### Stage 0 — land the mechanism (done)

`Permissive` default, chokepoints inside `native-api` wired, public API for the
rest. Zero behaviour change. Verified by the unit test
`no_capability_policy_means_no_gate_at_all`.

### Stage 1 — wire the gates (items 1-25 above), still `Permissive`

Still zero behaviour change: a gate with no policy installed returns `Ok`, and a
policy in `Permissive` mode allows. The deliverable is that
`capability_audit(vm)` becomes *complete* rather than partial.

**Exit criterion:** the audit report for a full suite run names every capability
in §1 that the suite exercises. A capability in §1 that never appears in a
report is an unwired gate, not an unused capability.

### Stage 2 — run everything in `Audit` and read the bill

`CRATONVM_CAPABILITY_MODE=audit` over the Spring Boot suite, the H2 suite, the
Tomcat suite, and CratonBench. `report.total_ungranted()` with an empty grant
set is the size of the problem; `report.suggested_grants()` is the first draft
of the answer.

### Stage 3 — grant in this order, because this is the order things break

1. **`native-register:*`** — breaks first, and hardest. ~3,100 registrations run
   at boot; refusing them yields a VM with no natives at all, which fails long
   before any application code runs. Anything narrower than `*` here is a
   per-build allow-list of every registered triple, which is a different project
   (the JDK-only census at `registry.rs` is the closest existing thing).
2. **`file-read:<java.home>`, `file-read:<classpath roots>`** — breaks second.
   Class loading, `java.home` property resolution, and resource lookup are all
   file reads; a VM that cannot read its own JDK image cannot bootstrap. Note
   the documented real-JDK bootstrap failure mode `InternalError: null property:
   java.home` (`registry.rs:4817`) as the shape of what this looks like.
3. **`file-write:<java.io.tmpdir>`** — breaks third. `File.createTempFile`,
   `FileHandler`, JAR extraction, and most test harnesses write to the temp
   directory.
4. **`raw-memory:*`** — breaks fourth. `java.nio.Bits`, direct `ByteBuffer`, and
   every `Unsafe`-backed collection in the JDK. A narrower grant is possible
   only once item 20 gives each raw-memory site a name.
5. **`file-read:<app data>` / `file-write:<app data>`** — application-specific;
   this is where the audit report earns its keep.
6. **`network:*`** then narrowed — servers bind before they connect, so grant the
   bind endpoints from the report first (item 23 covers those sites), then the
   outbound set.
7. **`process-spawn`** and **`library-load`** — most workloads need neither.
   These are the two that a real deployment should be able to leave ungranted,
   and they are the highest-value grants to withhold.
8. **`foreign-downcall` / `foreign-upcall`** — already secure-by-default through
   `NATIVE_ACCESS_POLICY`, so these are the cheapest to make default-deny. They
   should be the *first* kinds whose default flips, not the last.

### Stage 4 — flip per kind, not all at once

`CapabilityMode` is per-VM and per-set, so the migration does not have to be
atomic. The recommended sequence is to add a per-kind default (not implemented
yet — it would be a `[CapabilityMode; CapabilityKind::COUNT]` on `CapabilitySet`
replacing the single `mode` field) and flip `ForeignDowncall`, `ForeignUpcall`,
`ProcessSpawn`, and `LibraryLoad` to `Enforce` first, leaving `FileRead`,
`FileWrite`, `Network`, `RawMemory` and `NativeRegister` permissive. That
captures most of the C2 risk for a small fraction of the breakage.

### Stage 5 — CI gate

A CI job running the suite with `CRATONVM_CAPABILITY_MODE=audit` and a checked-in
grant file, asserting `report.total_ungranted() == 0`, turns any new ungated or
newly-privileged native into a test failure. This is the same shape as the
JDK-only gate's `synthetic_stub_invocations == 0` assertion.

### What specifically breaks if you flip the default *now*

| Flip | First failure |
|---|---|
| `Enforce` with empty grants | VM never boots: the first `register()` call is refused, and every subsequent native is missing |
| `Enforce` with `native-register:*` only | Class loading fails: no `file-read` grant for `java.home` / the classpath |
| `Enforce` with the above plus file reads | Every test using a temp file fails; `FileHandler`, `createTempFile`, JAR extraction |
| `Enforce` with all file grants | Direct `ByteBuffer` and `Unsafe`-backed JDK collections fail (`raw-memory`) |
| `Enforce` with everything but `network` | Every socket test, every embedded server, the whole Tomcat/Jetty/WildFly surface |

---

## 7. Per-VM versus process-global, and the `setSecurityManager` singleton

### 7.1 The singleton — **FIXED**

> **Status: this finding is closed.** The three process-global singletons in
> `native-builtins/src/security_manager.rs` are gone. What follows describes
> the defect as found, then the shape that replaced it — both are kept because
> the *reasoning* is what generalises to the globals in §7.3 that are still
> process-wide.

**As found.** Three process-wide statics, each
`Mutex<Option<(i32, ObjectRef)>>`: `SECURITY_MANAGER` (written by
`System.setSecurityManager`), `ACTIVE_POLICY_OBJECT` (written by
`Policy.setPolicy`), and the shared `Permissions` collection. The
`ACTIVE_POLICY_OBJECT` comment stated the intent — *"The singleton is
process-wide… that mirrors real JDK behaviour and is intentional."* — which is
correct for a **one-VM-per-process** JVM and wrong for an embedding.

In an embedding (`cratonvm-embed`, `libcratonvm`) with two `Vm`s in one process,
the consequences were concrete:

1. **Cross-VM policy interference.** VM A calling `System.setSecurityManager(sm)`
   installed an `ObjectRef` into VM B's gate as well. `check_exec_or_throw`
   read the singleton and, on a hit, called
   `ctx.invoke_virtual(sm_ref, "checkExec", …)` — invoking **VM A's heap object
   through VM B's context**. Not merely a policy leak; a cross-heap `ObjectRef`
   use.
2. **Privilege escalation by removal.** VM A calling
   `System.setSecurityManager(null)` disarmed VM B's `Runtime.exec` and
   `System.loadLibrary` gates, because both consult
   `get_security_manager(...).is_none()` and return `Ok(())` on `None`.
3. **GC coupling.** The singleton stored `(identity_key, ObjectRef)` and re-read
   the current address through `read_var_handle_root` on *the calling context*.
   The var-handle-root registry is **per-VM**, so with two VMs the key was
   registered in one and looked up in the other — a miss, and the code fell back
   to the **stale raw ref** (`read_var_handle_root(key).unwrap_or(cached)`).
   That address belongs to a heap the reading VM's collector never scans and the
   owning VM's collector cannot rewrite (it rewrites the registry entry, not the
   static copy). Under a moving young GC it is a use-after-move — exactly the
   unrewritable holder `docs/threading/objectref-concurrency-contract.md`
   forbids.

**The fix, in three parts:**

* **A per-VM index.** `SECURITY_STATE:
  OnceLock<Mutex<HashMap<usize, VmSecurityState>>>`, keyed by
  `NativeContext::vm_identity()`. `VmSecurityState` holds all three slots
  (`security_manager`, `policy_object`, `shared_permissions`) as
  `Option<(i32, ObjectRef)>`, reached only through `Slot`-named accessors
  (`security_slot` / `set_security_slot`) that take the key **from `ctx`, never
  from a caller**. A cross-VM read is therefore unrepresentable at the call
  sites. Clearing the last occupied slot drops the VM's row rather than leaving
  an all-`None` shell. This is deliberately the same shape as
  `native-api/src/capability.rs`'s `VM_CAPABILITIES`: *an index, not a policy.*
* **A real GC root source.** `gc_scan_security_manager_roots` /
  `gc_update_security_manager_refs` (the `lang_math::gc_scan_value_of_cache_roots`
  shape) are registered as the `"security-manager"` source in
  `vm/src/memory/native_roots.rs`, alongside `scan_security_manager` /
  `remap_security_manager`. The cached copy is now collector-**visible** and
  collector-**rewritable**, which is what closes finding 3 — per-VM keying alone
  would not have.
* **A stated lock discipline.** The state lock is never held across a Java
  allocation or any other re-entry into the VM, because the scan callback takes
  the same lock at a safepoint; the two lazy initialisers allocate first and
  publish afterwards.

**Still process-global (§7.3 territory, unchanged):**
`set_native_access_enabled` (`panama.rs`) and `set_path_confine_to_cwd` are
still process-wide setters, so a permissive embedder and a hardened one still
cannot coexist. `CapabilitySet` is the per-VM home those decisions move to;
items 10-25 of §5 are that migration, and the `security_manager.rs` rework above
is the worked example of what each of them looks like when done.

**Code comments that still cite this as open** (not edited here — outside this
doc's ownership): `native-api/src/capability.rs:15` (the mechanism table's
`SECURITY_MANAGER` row and the "cross-VM policy interference" consequence at
`:23-26`) and `native-api/src/registry.rs:2087` (`read_var_handle_root`'s
rationale, which lists `SECURITY_MANAGER` among the caches that must re-read
through it).

### 7.2 What is per-VM in the new model

| Thing | Scope | Enforced by |
|---|---|---|
| grants | per VM | `CapabilitySet` is owned by its VM; no `Default`, no `current()` |
| mode | per VM | field on `CapabilitySet`; `from_env` is called per VM, not latched in a `OnceLock` |
| audit log | per VM | field on `CapabilitySet`; unit test `two_vms_with_conflicting_grants_do_not_interfere` |
| registration gate | per VM | field on `NativeMethodRegistry`; unit test `two_registries_do_not_share_a_capability_policy` |
| dispatch classification | per VM | `sensitive_slots` map on the registry |
| `VmId -> CapabilitySet` index | process-global **index** | every accessor requires a `VmId`; two VMs get two entries |

The one remaining process-global is the index itself, and it is deliberate: a
native holding only a `&dyn NativeContext` needs *some* way to reach its VM's
policy before every `NativeContext` implementor carries one. The alternative —
adding `fn capabilities(&self) -> Option<&CapabilitySet>` to the `NativeContext`
trait — is strictly better and should replace the index once the VM owns a set
(§5 item 3 is the prerequisite). The index cannot leak one VM's policy to
another, because there is no expression that reads it without naming a VM.

---

## 8. What an untrusted class can still reach today

**With the default configuration** (`CRATONVM_CAPABILITY_MODE` unset, no
`SecurityManager` installed, `CRATONVM_CONFINE_IO` and `CRATONVM_UNTRUSTED_CODE`
unset, `--enable-native-access` not passed), a class loaded from the application
classpath can:

* **Read any file the host process can read**, at any absolute path, through
  `Files.newInputStream` / `FileChannel.open` / `FileInputStream`. Path traversal
  via a leading `..` is blocked by `has_escaping_parent_segment`; an absolute
  path is not blocked at all, and the `java.nio.file` natives in
  `native-builtins` do not call `validate_path` at all (gap **I2**).
* **Write, create, truncate, rename and delete any file the host process can**,
  same paths, same absence of checks.
* **Open outbound TCP and UDP connections to any host and port**, except the
  cloud-metadata link-local addresses the default outbound policy rejects
  (`outbound_policy.rs`). TLS connections likewise.
* **Bind and accept on any local address and port** (gap **I6**).
* **Spawn any host process**, through `ProcessBuilder.start` or any
  `Runtime.exec` overload. `check_exec_or_throw` is a no-op with no
  `SecurityManager` installed, and `validate_spawn_program` returns `Ok` for any
  program when confinement is off — which is the default.
* **Load any native library** by absolute path via `System.load`.
  `check_host_native_access_or_throw` returns `Ok` when no `SecurityManager` is
  installed.
* **Read and write raw process memory at an arbitrary address** via
  `Unsafe.getLong(J)` / `putLong(JJ)` / `getByte` / `putByte` for any address the
  arena store does not recognise, through the `real_ptr_read` / `real_ptr_write`
  fallback (gap **M2**). This is the single most powerful item on this list: it
  subsumes every other entry, because it can rewrite the gates themselves.
* **Register an FFM upcall stub** exposing a Java method to native code
  (`pe_upcall_handle`, gap **F3**), and invoke one (`pe_upcall_invoke`, gap
  **F4**), with no native-access gate.
* **Create an FFM downcall handle** (gap **F1**) — though *invoking* it is
  correctly refused unless `--enable-native-access` was passed.

What it **cannot** do by default:

* Invoke an FFM downcall (`pe_downcall_invoke` → `require_native_access`).
* Wrap a non-zero raw address as a `MemorySegment` or `reinterpret` one.
* Read or write through a raw-address `MemorySegment`.
* Use `SymbolLookup.libraryLookup`.

In other words: **the FFM surface is the one part of the native API that is
already secure-by-default, and it is also the only part with a documented,
explicit policy object.** Everything else is open. That asymmetry is the single
clearest argument for this work — the FFM gate proves the pattern works, and §5
is the list of places to apply it.

### 8.1 With `CRATONVM_UNTRUSTED_CODE=1`

The existing hardening profile (`native-io/src/lib.rs:194`) closes some of this:
CWD confinement is forced on and fails closed, host native access is denied
(`security_manager.rs:75`), and `--enable-native-access` is refused
(`panama.rs:124`). It does **not** close I2 (the `native-builtins` `nio_file`
natives bypass `validate_path` entirely), I6 (server socket bind), M2 (raw
`Unsafe` addresses), F3/F4 (upcalls), or any network destination beyond the
metadata addresses.

---

## 9. Tests

`native-api/src/capability.rs` (unit):

* path normalization (separator folding, `.`, interior `..`, absolute clamping,
  relative escape marking), Windows case-insensitivity under `cfg(windows)`;
* **path traversal cannot escape a granted prefix** — `/data/../etc/passwd`,
  `/data/sub/../../etc/passwd`, and the backslash spelling of each;
* shared-prefix siblings (`/data` must not admit `/database`);
* host globs (`*`, `*.example.com` including the bare suffix domain, the
  `notexample.com` near-miss), port exact/range, IPv6 literals, case folding;
* fail-closed on an unnameable port and on an `Any`-scoped request;
* each mode's behaviour, including that `Audit` allows but tallies;
* **per-VM isolation** — two sets with conflicting grants, separate audit logs,
  and different modes;
* denial content (kind, scope, VM, `#[track_caller]` line) and the
  `SecurityException` conversion;
* grant-list parsing including bad entries, and every kind's round trip;
* the audit report: determinism, first-site (not last-site), the bound and its
  `truncated` flag, and that `suggested_grants()` produces a grant set which
  admits everything the run did and nothing else;
* `classify_native` coverage and its negatives (`Runtime.gc` must not classify).

`native-api/src/registry.rs` (unit): no-policy is a no-op; `Permissive`
classifies but never refuses; `Enforce` refuses a registration without moving
the generation or the census; `Enforce` gates dispatch of a classified native;
re-registration keeps the slot's classification correct; two registries do not
share a policy.

`native-api/src/fd_table.rs` (unit): permissive checked openers behave like the
raw ones; a denied read consumes no descriptor; a denied write does not create
the file; a granted prefix admits files under it and refuses a traversal out of
it; read-write open needs both capabilities; network openers are gated on the
endpoint and a portless UDP bind fails closed; a denial maps to
`PermissionDenied` for `io::Error`-only call sites.

---

## 10. Related

* `docs/SECURITY_HARDENING.md` — the existing `CRATONVM_CONFINE_IO` /
  `CRATONVM_UNTRUSTED_CODE` profiles.
* `docs/security/jdk-only-threat-model.md`.
* `docs/feature-designs/jdk-only-mode.md` §2 — the "no process globals for a
  per-VM feature" rule this module follows.
