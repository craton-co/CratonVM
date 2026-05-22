# Feature Roadmap — Interpreter Intrinsic Table

Status: **Proposed** · Owner: _unassigned_ · Target: post-0.2.0

## 1. Goal

Give the **interpreter** a fast path for hot JDK methods so that calls such as
`String.length`, `StringBuilder.append`, `System.arraycopy`, and
`Object.getClass` no longer pay a full native-registry lookup on every
invocation.

The mechanism: an intrinsic table keyed by `(class, name, descriptor)` that is
**resolved once, at inline-cache fill time**, and thereafter dispatched as a
direct `fn` pointer (or a small enum tag) — no `RwLock`, no descriptor parsing,
no hash probe on the steady-state path.

## 2. Current state (baseline)

- There is **no** general interpreter intrinsic table.
- The interpreter already has a **monomorphic inline cache** for
  `invokevirtual`/`invokeinterface` — `vm/src/runtime/interpreter.rs:4402`
  (and `:4541`). This is the natural place to cache an intrinsic resolution.
- Every hot native method today goes through the general invoke path:
  `execute_invoke` → native-registry lookup (`native_methods.find`,
  `interpreter.rs:1375` / `:1544`) → `safe_native_call`. Each call pays:
  - a `RwLock` read on the class manager,
  - descriptor parsing,
  - a `FxHashMap` probe on the 128-bit native-method key.
- The only existing special-case is a single hardcoded `Math.sqrt` check in the
  **JIT-prep scan** (`interpreter.rs:1739`) — that feeds the JIT, it is **not**
  an interpreter execution fast path.
- The Rust implementations themselves already exist and are correct
  (`native-builtins`: `lang_string.rs`, `lang_system.rs`, `lang_math.rs`, …).
  This roadmap optimizes **dispatch only** — it does not add new behavior.

## 3. Design

### 3.1 Intrinsic identity

Define an `InterpIntrinsic` enum (one variant per supported method) and a
build-time/`OnceLock` table:

```rust
static INTRINSIC_TABLE: phf::Map<(&str,&str,&str), InterpIntrinsic> = …;
```

keyed on `(class, name, descriptor)`. Resolution is a single `phf` probe,
performed **once per call site**.

### 3.2 Inline-cache integration

Extend the existing monomorphic IC entry (`interpreter.rs:4402`) with an
`Intrinsic(InterpIntrinsic)` state alongside the existing
`Resolved(method_ptr)` state:

```
IC state machine per call site:
  Empty ──first hit──▶ resolve:
      (class,name,desc) in INTRINSIC_TABLE ? ──▶ Intrinsic(kind)
      else                                  ──▶ Resolved(method)/Native(ptr)
  Intrinsic(kind) ──steady state──▶ direct call to the intrinsic handler
```

On a megamorphic / receiver-class-mismatch event the entry degrades to the
normal path (the intrinsic table is keyed on the *static* target, so for
`invokevirtual` the IC must still verify the resolved receiver class — see
§3.4).

### 3.3 Dispatch

Each `InterpIntrinsic` variant maps to a Rust handler with a uniform signature
(operands from the operand stack, `&mut dyn NativeContext`, returns the result
or a `MethodCallFailed`). The handler is invoked directly from the interpreter
loop — bypassing `native_methods.find`, the class-manager `RwLock`, and
descriptor parsing entirely.

### 3.4 Correctness guards

- **Virtual dispatch:** for `invokevirtual`/`invokeinterface`, an intrinsic is
  only valid if the *actual* receiver class is the class the intrinsic was
  resolved for (or a subclass that does not override the method). The IC must
  guard on the receiver's class id; on mismatch, fall back to normal dispatch.
  `invokestatic` calls have no such concern.
- **Exact-match only:** the table is keyed on the full descriptor — overloads
  resolve independently; no fuzzy descriptor matching.
- The intrinsic handlers must call into the **same** `native-builtins` code (or
  be byte-for-byte equivalent), so behavior cannot diverge from the slow path.
- Respect `synthetic-jdk` / real-JDK mode: the table must produce results
  identical to whichever stdlib the VM is running.

## 4. Initial intrinsic set

Phase 1 (leaf, no allocation, highest call frequency):

- `java/lang/Object` — `getClass ()Ljava/lang/Class;`, `hashCode ()I`.
- `java/lang/String` — `length ()I`, `charAt (I)C`, `isEmpty ()Z`.
- `java/lang/System` — `arraycopy (Ljava/lang/Object;ILjava/lang/Object;II)V`.

Phase 2:

- `java/lang/StringBuilder` / `StringBuilder` — `append` (common descriptors),
  `toString`, `length`.
- `java/lang/Integer` / `Long` — `valueOf`, `intValue`, `longValue`,
  `parseInt`/`parseLong` fast paths.
- `java/lang/Math` scalar ops already covered by the JIT — add interpreter
  fast paths for the interpreter-only (un-JIT-compiled) case.

## 5. Phases

- **Phase 0 — IC plumbing.** Add the `Intrinsic` IC state + the `phf` table
  scaffold with an empty set. No intrinsics yet; prove the IC degrades correctly
  and benchmarks are unchanged.
- **Phase 1 — Leaf set.** `Object.getClass/hashCode`, `String.length/charAt/
  isEmpty`, `System.arraycopy`. Establish the differential-test harness.
- **Phase 2 — StringBuilder + boxing.** `append`, `toString`, `Integer/Long`
  box/unbox.
- **Phase 3 — Profiling & tuning.** Confirm the IC guard cost for virtual
  intrinsics does not erase the win; consider a polymorphic (2-way) IC if
  needed.

## 6. Files to touch

- `vm/src/runtime/interpreter.rs` — IC entry extension (`:4402`, `:4541`),
  the `InterpIntrinsic` enum, the dispatch in `execute_invoke`.
- New module, e.g. `vm/src/runtime/intrinsics.rs` — the `phf` table + handlers
  (or thin wrappers delegating to `native-builtins`).
- `native-api` / `native-builtins` — expose the underlying implementations as
  directly-callable functions if they are currently only reachable via the
  registry.

## 7. Testing

- **Differential tests:** every intrinsic must return exactly what the
  registry/native path returns over a randomized input matrix — run each test
  twice (intrinsics on / forced off via a debug flag) and assert equality.
- **Virtual-dispatch guard tests:** a subclass that overrides `hashCode` /
  `length` must NOT hit the intrinsic; confirm IC fallback.
- **Megamorphic test:** a call site that sees many receiver classes degrades
  cleanly and does not livelock the IC.
- **Exception parity:** `arraycopy` still throws `NullPointerException` /
  `ArrayStoreException` / `ArrayIndexOutOfBoundsException` identically.

## 8. Risks

- **Virtual-dispatch unsoundness** — the single largest risk. An intrinsic that
  ignores receiver-class overrides would call the wrong method. Mitigation: the
  IC class-id guard in §3.4, plus the subclass-override tests.
- **IC guard cost** — for virtual intrinsics the class-id check could approach
  the cost it saves on very short methods. Mitigation: Phase 3 profiling; keep
  `invokestatic` intrinsics (no guard) as the guaranteed win.
- **Behavior drift** from the slow path — mitigated by reusing `native-builtins`
  code and the on/off differential tests.

## 9. Acceptance criteria

- Steady-state intrinsic call: no `RwLock` acquisition, no `HashMap` probe, no
  descriptor parse (verified by profiling / counters).
- Differential tests pass with intrinsics on vs. off; no JCK regressions.
- Measurable speedup on an interpreter-bound, call-heavy benchmark (target:
  ≥1.3× on a `String`/`arraycopy`-heavy loop run with the JIT disabled).
