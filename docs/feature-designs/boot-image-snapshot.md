# Boot Image Snapshot Architecture (Fast Cold Start)

## Status

**Shipped (opt-in via `CRATONVM_BOOT_IMAGE=capture|auto`).**

To achieve sub-10ms cold-start execution under compatible mode (`--compatible`), CratonVM incorporates a boot image snapshot mechanism. Instead of repeating the full initialization sequence—loading and defining hundreds of boot classes, parsing module descriptors, and initializing runtime state on every launch—the virtual machine can restore pre-booted state directly from an on-disk image.

---

## 1. Architectural Motivation

A complete, authentic OpenJDK boot sequence involves:
- Loading and verifying core system classes (`java.lang.Object`, `Class`, `String`, etc.).
- Registering thousands of native method bindings.
- Executing interpreter initialization routines (`System.initPhase1`, module system layer setup).

Under compatible mode (`--compatible`), this cold startup path requires substantial instruction execution. Boot image snapshotting decouples startup latency from class count by capturing the state of an initialized VM once and restoring it via copy-on-write memory mappings.

---

## 2. Capture and Restore Architecture

```text
[ Capture Run ]
  Prologue (P0)  --->  Boot Sequence  --->  Snapshot Point (P1)  --->  Write Image File
  (Track mappings)     (Capture arena)      (Single-threaded)          (Data diff & arena)

[ Restore Run ]
  Prologue (P0)  --->  Map Image (Fixed) ---> Rebind Identity   --->  Execute Target Main
  (Verify key)         (Apply data diff)     (TLS, threads, env)
```

### The Snapshot Sequence

1. **Prologue (P0)**: The launcher begins execution, computes the cache key, and determines whether an existing valid snapshot is present.
2. **Snapshot Point (P1)**: The capture run completes bootstrap initialization up to the point where the JVM is fully initialized (`boot_publish_jdk_booted`), before user application code executes. Background threads are deferred, ensuring only the primary launcher thread and `main-vm` exist.
3. **Imaged Subsystems**:
   - **Capture Arena**: Memory allocations made during boot are directed into a dedicated fixed-address capture arena.
   - **Fixed Address Mappings**: VM mappings (garbage collection heap reservations, card tables, JIT code pages, and open `jimage` file handles) are coordinated through an address broker (`types/src/image_layout.rs`). File-backed mappings are re-mapped on restore; anonymous pages are serialized into the snapshot.
   - **Static Data Differentiator (`.data` / `.bss`)**: Global state changes occurring between P0 and P1 are recorded as a sparse binary diff against the base executable.
   - **Validation Key**: Contains the launcher binary build-id, JDK image checksum, classpath identities, active VM configuration flags, and host architecture attributes.

### Fast Restore Sequence

On subsequent invocations with matching configuration:
1. **Verification**: The snapshot key is checked against current execution parameters. If any component (class library identity, JVM flags, or binary version) differs, the launcher automatically falls back to a standard cold boot.
2. **Mapping**: The image file is mapped into memory (`MAP_PRIVATE | MAP_FIXED`), allowing pages to be demand-paged on write.
3. **Data Diff Application**: Captured modifications to static variables and thread-local state are applied.
4. **Process Identity Rebinding**: Runtime process state is updated:
   - Operating system thread IDs (`tid`) and execution stack bounds.
   - Signal handlers and runtime trap vectors.
   - High-resolution process start clocks and timestamps.
   - Per-launch system properties (`sun.java.command`, application arguments).
5. **Deferred Thread Activation**: Asynchronous VM threads (such as reference handlers or compiler workers) are spawned, and execution transitions immediately into application `main`.

---

## 3. Safety and Validation Gates

To ensure restored execution is completely identical to a fresh boot, the snapshot mechanism enforces strict safety checks:

- **Static Variable Classification**: During capture, every modified byte in `.data` and `.bss` is mapped to its containing symbol and verified against `boot_image_statics.txt`. Unclassified global state causes capture to abort.
- **Pointer Scanning**: Before writing the image, memory regions are scanned to ensure no pointers reference volatile host memory or transient heap regions outside the managed arena.
- **Fail-Safe Fallback**: Any validation error, corrupted image, or memory mapping conflict triggers an immediate, silent fallback to normal cold boot execution.

---

## 4. Configuration and Diagnostics

| Environment Variable | Values | Description |
|---|---|---|
| `CRATONVM_BOOT_IMAGE` | `off` | Snapshotting disabled (default). Standard cold boot. |
| | `capture` | Forces generation of a boot snapshot to disk, then exits. |
| | `auto` | Restores from existing valid snapshot if available; otherwise falls back to cold boot. |
| | `verbose` | Logs snapshot verification and restoration timing. |
| `CRATONVM_BOOT_IMAGE_DIR` | `<path>` | Custom directory path for caching boot image files. |
