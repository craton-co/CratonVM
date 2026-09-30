# Class Loading & Verification

Class loading implements the JVM specification's loading, linking, and
initialization model (JVMS Chapter 5). It lives in the `cratonvm-classloading`
crate, with the `.class` parser in `cratonvm-reader`.

## The reader

`cratonvm-reader` is a pure `.class` file parser with no VM dependencies — it can
be used on its own to inspect class files. It parses per JVMS Chapter 4:

| Module | Parses |
|--------|--------|
| `class_reader.rs` | Entry point: bytes → `ClassFile`. |
| `buffer.rs` | A big-endian binary cursor. |
| `constant_pool.rs` | The 20 constant-pool entry types. |
| `instruction.rs` | 200+ bytecode opcodes. |
| `attribute.rs` | 30+ attribute types. |
| `stack_map.rs` | `StackMapTable` verification frames. |
| `field_type.rs` / `method_descriptor.rs` | Field and method descriptors. |
| `class_access_flags.rs` / `class_file_version.rs` | Access flags and version constants (Java 1.1–25). |

## Loading, linking, initialization

| Module | Responsibility |
|--------|----------------|
| `class_manager.rs` | Central class cache and loading coordinator. |
| `loaders.rs` | Bootstrap, extension, and application class loaders. |
| `class_path.rs` | Classpath scanning (directories + JARs). |
| `class.rs` | The runtime class representation (metadata + hierarchy). |
| `resolution.rs` | Symbolic-reference resolution, with inline caching. |
| `access_control.rs` | Access checks (JVMS 5.4.4). |

The lifecycle is the standard one: a class is **loaded** (parsed and registered),
**linked** (verified, prepared, references resolved as needed), and
**initialized** (static initializers run) lazily on first active use.

## Verification

Bytecode verification happens by default (matching HotSpot's `remote` policy:
non-boot classes are verified). It has two layers:

| Module | Checks |
|--------|--------|
| `verifier.rs` | Structural verification (JVMS 4.8–4.9). |
| `bytecode_verifier.rs` | Type-checking verification (JVMS 4.10), using a verification type lattice (`vtype.rs`). |

Modern class files carry `StackMapTable` attributes and verify with the
type-checking verifier. Pre-Java-7 class files relying on the legacy split
verifier are an area of ongoing completeness work. Verification can be disabled
with `--noverify` (not recommended) — see
[Configuration](../user-guide/configuration.md).

## Real JDK modules vs. synthetic classes

When a JDK is detected, the class manager loads real standard-library classes
from the JDK's modules (`java.base.jmod`, or `lib/modules` on a jlink image). In
synthetic mode, standard-library classes are provided by CratonVM's native
implementations instead. See [JDK Modes](../getting-started/jdk-modes.md). The
module path and `--add-*` flags feed the same class-loading machinery — see
[Modules (JPMS)](../user-guide/modules.md).

## Resolution & inline caching

Symbolic references (to classes, fields, and methods) are resolved on demand and
cached, so repeated access at a call site doesn't re-resolve. The resolution
cache is bounded (configurable via `CRATONVM_RESOLVE_CACHE_CAP`) to keep its
footprint stable.
