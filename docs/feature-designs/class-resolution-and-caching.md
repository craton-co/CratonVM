# Class Resolution, Caching, and Verification Architecture

**Status:** Shipped (default on). The class loading, resolution, and caching pipeline
operates across all execution modes (`--jdk-only` and `--compatible`).

## What it does today

CratonVM's class loading subsystem (`classloading/src/class_manager.rs`,
`classloading/src/class_path.rs`) coordinates class parsing, verification, hierarchy
construction, and symbol resolution while ensuring JVM specification conformance and
high-performance caching.

### 1. Classpath and Archive Scanning

- **JAR and Fat-JAR Caching:** Classpath entries, including multi-release JARs and shaded
  fat-JARs, are indexed and cached by path and filesystem stamp (`mtime` + length).
- **Stale Entry Eviction:** If an archive file is modified on disk, stale cached walk rows
  are evicted to ensure consistent class resolution across restarts or dynamic appends.
- **Duplicate Name Disambiguation:** Path ordering governs entry precedence. Duplicate
  package locations across boot append entries and application jars are resolved deterministically.
- **Boot Append Package Location Tracking:** Dynamic additions via `-Xbootclasspath/a:` or
  JVM TI instrumentation record package locations immediately upon append to preserve module
  system access boundaries.

### 2. Loader Constraints and Multi-Loader Safety

- **Initiating vs Defining Loaders:** For every class resolution, the initiating loader
  delegates up the hierarchy (Boot -> Platform -> System/App -> Custom). Once loaded,
  the defining loader is recorded as the authoritative owner of the class runtime representation.
- **Loader Constraint Table:** When a method signature or field references a class type across
  different initiating loaders, loader constraints are recorded in the per-VM constraint table
  (`loader_constraint_table`). This guarantees that both loaders resolve the symbolic name
  to the exact same loaded `Class` instance.
- **Constraint Lifetime:** Constraint entries are reference-counted with respect to the
  lifetimes of the involved class loaders. When a class loader is unloaded during GC class
  unloading sweeps, associated loader constraint rows are pruned to prevent memory leaks.

### 3. Vtable, Itable, and Dispatch Layout

- **Vtable Construction:** Each loaded class constructs a virtual method table (vtable)
  inheriting and overriding superclass method slots. Subtype check displays (`display_table`)
  are populated to provide O(1) subtype checks for primary superclasses.
- **Itable Indexing:** Interface method tables (itables) map interface method offsets to
  concrete implementation stubs. Interface conflicts (e.g., default method collisions) are
  resolved per JVM specification rules.
- **Single Vtable Copy Invariant:** Class metadata maintains a unified vtable descriptor
  structure to minimize memory footprint and eliminate duplicate copies across subclass hierarchies.

### 4. Dynamic Classes: Hidden Classes and Lambdas

- **Hidden Classes (`Lookup.defineHiddenClass`):** Hidden classes are generated dynamically
  with unguessable synthetic names. They are attached to the nest host of the defining class
  and can be configured with independent class-loader lifetimes.
- **Call-Site Resolution:** Constant pool resolution handles `invokedynamic` bootstrap
  methods, CallSite linking, and MethodHandle adaptation caching.

### 5. Bytecode Verification and Linking

- **Two-Walk Verification:** Classes are verified using two-pass structural verification:
  first validating class file format, constant pool integrity, and method bytecodes, followed
  by stack map table verification (`StackMapTable`) ensuring operand stack and local variable
  type safety.
- **Define-Time Type Maps:** Type hierarchies and linkage dependencies are resolved at link
  time to allow immediate JIT compilation eligibility without redundant interpreter tracing.

### 6. Dynamic Class Redefinition (`RedefineClasses`)

- **Invoke Cache Invalidation:** When classes are redefined via JVM TI `RedefineClasses`,
  a global redefinition counter is incremented, invalidating cached resolved call-site
  targets and inline cache descriptors.
- **Obsolete Method Tagging:** Running activations of older method versions are marked as
  obsolete. Return paths and deoptimization points gracefully handle transitions back to
  redefined code.
