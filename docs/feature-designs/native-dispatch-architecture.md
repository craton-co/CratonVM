# Native Dispatch Architecture — Categories, Scoping, and Mode Enforcement

**Status:** Shipped (default on). Governs native registration, dispatch resolution,
and mode isolation across `--jdk-only` and `--compatible`.

## What it does today

CratonVM's native method subsystem (`native-api/src/registry.rs`, `vm/src/vm/vm_exec.rs`)
provides a structured, category-enforced interface between Java bytecode execution and
Rust native implementations.

### 1. The `NativeKind` Hierarchy and Registration Scoping

Every registered native function is explicitly tagged with a category:

- **`Intrinsic`:** A high-performance replacement specifically reviewed and approved to
  supersede authentic OpenJDK bytecode (e.g., CRC32, array copy, crypto primitives,
  math intrinsics).
- **`Bridge`:** A genuine native bridge connecting Java APIs to host OS functionality
  (e.g., file descriptors, sockets, thread management, memory allocation). Bridges are
  valid in both `--jdk-only` and `--compatible` modes.
- **`SyntheticStub`:** A compatibility stub used in `--compatible` mode to bypass complex
  JDK initialization paths or supply fast synthetic behavior. **Synthetic stubs are strictly
  forbidden in `--jdk-only` mode.**

**Ambient Category Discipline:** The registry's default ambient category is `SyntheticStub`.
Every registrar must explicitly configure its category scope (`with_category` / `set_category`)
and restore it on exit. Registrations outside an explicit category scope inherit `SyntheticStub`
and are dropped under `--jdk-only` / `CRATONVM_NO_STUBS`. A static ratchet test
(`native-builtins/tests/stub_ratchet.rs`) freezes the synthetic stub count to prevent unclassified
additions.

### 2. Dispatch Resolution (`resolve_dispatch`)

The dispatch resolution logic in `vm/src/vm/vm_exec.rs` follows strict precedence:

1. **Intrinsics:** An active, reviewed `Intrinsic` takes precedence.
2. **Authoritative Bytecode:** Authentic Java class bytecode is authoritative over registered
   natives. If real class bytes exist for a method, they beat a registered `Bridge` or
   `SyntheticStub`.
3. **Bridges:** In `--jdk-only` mode, if bytecode is native, only approved `Bridge` (or `Intrinsic`)
   implementations are dispatched. Any `SyntheticStub` registration triggers an immediate refusal.
4. **Compatible Mode Stubs:** In `--compatible` mode, synthetic stubs and collection interceptions
   provide fast cold-start execution paths.

### 3. Native Handle Discipline (`HandleScope`)

To prevent garbage collection sweeps from collecting objects referenced during native method
execution:

- Native methods that allocate heap memory or execute nested Java calls must enclose object
  references inside scoped handle guards (`HandleScope`).
- Handles register object references as temporary GC roots for the duration of the native activation.
- Upon exiting the handle scope, root registrations are dropped automatically via RAII, preventing
  root accumulation or memory leaks.

### 4. Per-VM State Isolation

- **No Process Globals:** Native method registries and class manager linkages are strictly
  bound to the owning `VmConfig` and VM instance.
- Multiple isolated VM instances may coexist within the same operating process with distinct
  native method registrations and compatibility policies.
