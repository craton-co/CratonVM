# ARCHITECTURE.md truth pass — 2026-07-26

Record of a verification pass over `ARCHITECTURE.md` (and the one affected
`README.md` bullet). Every claim below was re-checked against the tree at
`dev` on 2026-07-26 before the doc was edited. Line numbers are from that
tree; re-verify before citing them, several of these files are under active
edit.

Scope note: this pass changed **documentation and one character-encoding
repair only**. No behaviour was changed by it.

---

## The pattern behind the errors

Most of `ARCHITECTURE.md` is accurate, and unusually candid about the VM's
weak spots — that quality is why the false claims stood out. The false ones
were not random. Four of the five share a single shape:

> A capability lands behind a `CRATONVM_*` environment variable, the variable
> defaults to **off**, the work is recorded as "implemented", and the code
> never executes on the default path.

The doc then describes a VM nobody is running. This is documented in the tree
itself — see `jit/src/x64.rs:2646-2662`, where the fix commentary states that a
protection several call sites claimed as theirs "only ran when the SEPARATE
`CRATONVM_JIT_SAFEPOINT_REG_SPILL` env var was ALSO set — off by default, so
the documented protection never actually happened."

Scale of the exposure, measured 2026-07-26 across the workspace crates:

| Measure | Count |
|---|---:|
| Distinct `CRATONVM_*` identifiers | 593 |
| Bare `std::env::var` call sites | 388 |
| Bare `std::env::var_os` call sites | 307 |
| Identifiers centralised in `types/src/flags.rs` | 248 (behind 9 reads) |
| Reads centralised in `vm/src/runtime/env_cache.rs` | 39 |

So roughly half the flag identifiers have a single, greppable default; the
other half do not. That asymmetry is the mechanism.

The countermeasure now lives in `ARCHITECTURE.md` as
**"How to tell whether a feature actually runs"** — a five-step reviewer
checklist plus the convention that new capabilities ship **opt-out**, never
opt-in, and that a doc sentence must name the default explicitly.

---

## Corrections made

### 1. Key Design Decision 1 — "No JDK dependency"

**Was:** "All standard library classes are implemented as native methods in
Rust. This means no `JAVA_HOME`, no `rt.jar`, and no dependency on any JDK
installation at runtime."

**Verified false for the default build.**

- `vm/Cargo.toml`: `synthetic-jdk` is **not** in `default = [...]`. Its comment
  reads "The default build boots against real JDK bytecode loaded from
  `$JAVA_HOME/lib/modules`".
- `vm/src/config.rs:503`, in `VmConfig::with_host_jdk_default` (the launcher
  path): `cfg.use_synthetic_jdk = detect_real_jdk().is_none();` — the mode is
  chosen by **sniffing the host machine**.
- The two modes are two complete standard libraries: real `java.base` bytecode
  from `jmods`/`lib/modules` plus an essential-native surface, versus the Rust
  stub library.
- The synthetic surface is itself partially feature-gated: ~120
  `#[cfg(feature = "synthetic-jdk")]` sites across `native-builtins`,
  `native-collections`, `native-io`, and `vm`. `--synthetic-jdk` on a default
  binary and a `--features synthetic-jdk` build are therefore not the same
  library.

**Now:** describes both modes, states which is the default, names the
host-sniffing selection site, and notes that making the mode explicit rather
than autodetected is tracked work. The follow-on sentence in the *Native
Methods* section ("Instead of loading `rt.jar`, CratonVM provides native Rust
implementations of JDK classes") carried the same falsehood and was corrected
to match.

`README.md` was already accurate here (it describes real-JDK-first with a
standalone fallback) and was left alone.

### 2. `vm/src/vm.rs` in the file-size discussion

**Was:** newcomers pointed at "`vm/src/vm.rs` (~68,000 lines)" as the largest
VM file.

**Verified misleading.** The file is **74,043** lines, but `mod tests` opens at
line 63 (its `#[cfg(all(test, feature = "synthetic-jdk"))]` attribute at 62) and
its closing brace is the last line of the file. Everything after line 61 is one
test module the default build does not compile.

The actual orchestrator is the ~61-line module header, which declares and
re-exports `vm/src/vm/`:

| File | Lines |
|---|---:|
| `vm/src/vm/vm_exec.rs` | 21,199 |
| `vm/src/vm/vm_init.rs` | 11,895 |
| `vm/src/vm/vm_util.rs` | 4,309 |
| `vm/src/vm/vm_object.rs` | 2,101 |
| `vm/src/vm/realms/` | (module tree) |

The two largest production files are `vm/src/runtime/interpreter.rs` (46,799)
and `jit/src/x64.rs` (40,955). The doc now says so, and says explicitly that
the test module is non-default-feature.

### 3. Quickening described as a future fix

**Was:** "…the slow path, which re-decodes every instruction on **every
execution** … A one-time link-time quickening pass is the tracked fix."

**Verified already landed.**

- `reader/src/quickened.rs` implements `QuickenedCode`.
- `vm/src/runtime/interpreter.rs:7872` — `fn quickened_for_frame(frame: &Frame)
  -> Option<Arc<cratonvm_reader::QuickenedCode>>`, consumed at `:10366`.

The **real residual** is the pc→index lookup, not re-decoding:
`QuickenedCode::index_of_pc` (`reader/src/quickened.rs:186`) resolves
straight-line execution in a single compare against a `hint`, but every taken
branch misses the hint and falls back to `self.pcs[..n].binary_search(&pc32)`.
The doc now states the fix as done, marks it "do not re-propose", and names the
binary search as the thing left to attack.

### 4. GC section understated the situation

**Was:** "The default is the generational semi-space collector (Cheney moving
young gen + non-moving sweep)."

**Verified: the moving path is off by default and, on a JIT-warm workload,
effectively unreachable.** Two independent gates:

1. `types/src/flags.rs:619` — `moving_young: present(src, "CRATONVM_MOVING_YOUNG")`.
   `present()` (`types/src/flags.rs:182`) is `src.get(name).is_some()`:
   presence is truth, so the default is **false**. Pinned by the
   `empty_source_matches_all_documented_defaults` test at `:1657`
   (`assert!(!f.gc.moving_young);`).
2. `gc/src/gen_heap.rs:3770` —
   `let fail_closed_non_moving = crate::gc_quiescence::is_active() && !gc_flags().allow_moving_young;`
   `is_active()` (`gc/src/gc_quiescence.rs:187`) is `active_depth_get() > 0`,
   incremented/decremented by `JitEntryGuard`, i.e. true whenever **any** thread
   is inside a JIT call. `allow_moving_young` (`types/src/flags.rs:620`) is
   `present()`-gated and also defaults false.

`CRATONVM_JIT_THRESHOLD` defaults to **500** (`vm/src/runtime/env_cache.rs:92`),
so a long-running workload spends most of its allocation time under a live JIT
frame. The collector's own trace confirms the outcome:
`"… — compaction deferred."` (`gc/src/gen_heap.rs:3798`).

The doc now states plainly that a default build gives generational **non-moving
mark-sweep with selective promotion**, that this is a different complexity class
from the one advertised, and cross-references
`docs/internal/arch-2026-07-26/moving-young-precise-roots.md` (companion doc
from the same wave) for the precise-root work the moving path needs.

`README.md`'s GC bullet did not make the compaction claim, but said nothing
about the default either; it now names the non-moving default and the opt-in
flag.

Also corrected in passing: Key Design Decision 7 said "~560 distinct
`CRATONVM_*` identifiers"; the measured figure is 593, and the doc now also
gives the call-site counts and the centralisation split.

### 5. New section — "How to tell whether a feature actually runs"

Added to `ARCHITECTURE.md` between *Key Design Decisions* and *Data Flow*. It
carries the confirmed-instance table (`moving_young`, `allow_moving_young`,
`use_compressed_oops`, `safepoint_reg_spill`, the `precise_maps` register-spill
half), the five-step reviewer checklist, and the opt-out convention.

Worth noting on the `safepoint_reg_spill` row: it is the one instance that has
**already been fixed**, and the fix is the template. `jit/src/x64.rs:2646-2662`
folded the spill into default-on `precise_maps` and inverted the flag to an
opt-**out** bisection switch (`CRATONVM_NO_PRECISE_REG_SPILL`). That is the
shape every other row should be moved to.

### 6. Compressed oops described as unwired (found during this pass)

Not on the original review list; found while checking the `use_compressed_oops`
row for the new section.

**Was:** "Compressed oops (`gc/src/compressed_oops.rs`) are implemented and
tested but **not wired into the live heap**… narrow-oop field layout, JIT
load/store barriers, GC root re-encoding, and klass-pointer compression are the
remaining work."

**Verified stale.** `gc::compressed_oops::enable_for_live_heap` is called from
`vm/src/vm/vm_init.rs:874`, and the module's own header
(`gc/src/compressed_oops.rs:19-37`) documents the current state: wired behind
an opt-in `-XX:+UseCompressedOops` / `CRATONVM_COMPRESSED_OOPS=1` gate, off by
default; reference instance fields and reference array elements narrow to 4
bytes; klass-pointer narrowing deliberately skipped because
`ObjectHeader::class_id` is already a `u32`. The gate stays off for a
*throughput* reason — the JIT's inline compact-field fast paths are disabled
while compressed oops are active — not because the wiring is missing.

This is the same failure mode running in the opposite direction: a default-off
flag caused the doc to under-report shipped work rather than over-report it.
Both errors come from describing capability without stating the default.

### 7. Character-encoding repair

`vm/src/vm.rs` and `vm/benches/vm_benchmarks.rs` carried UTF-8-through-cp1251
mojibake in comments and one string literal — double-round corruption
(UTF-8 → cp1251 → UTF-8 → cp1251 → UTF-8), which is why an em dash appeared as
the 7-character sequence `РІР‚вЂќ`.

Repaired, comment/string text only, no code or formatting touched, CRLF count
asserted unchanged (74,043 / 1,165):

| Repaired to | `vm.rs` | `vm_benchmarks.rs` |
|---|---:|---:|
| `—` (em dash, double-round) | 217 | 5 |
| `—` (em dash, single-round) | 2 | 0 |
| `→` | 198 | 7 |
| `←` | 1 | 0 |
| `…` | 1 | 0 |
| `§` | 1 | 0 |
| `≈` | 1 | 0 |
| `日本語テスト` (string literal in `create_unicode_string`) | 2 | 0 |
| `—` reconstructed from `U+FFFD ×3` | 1 | 0 |

Two items need flagging:

- **`vm/src/vm.rs:66797`** held three `U+FFFD` replacement characters — the
  original bytes are *destroyed*, not merely re-encoded, so no mechanical
  decode could recover them. The line is a section header,
  `// M22 ??? RSA/ECDSA signing end-to-end`, and every sibling header in the
  file uses the form `// M7 — Panic site hardening`, `// M2 — Default interface
  methods`, etc. An em dash was substituted **by reconstruction from that
  pattern**, not by decoding. If the original was something else, this is where
  to look.
- **`vm/src/vm.rs:273,277`** are a Java string literal, not a comment:
  `create_java_string(&shared, "日本語テスト")` in the `create_unicode_string`
  test. The test asserts a round-trip through the heap, so it passed against
  the corrupted text as happily as against the correct text — worth knowing
  that the test does not guard this.

`vm/Cargo.toml` carries the *terminal* form of the same corruption — literal
ASCII `???` where an em dash belongs, at lines 17, 49, and 56 — and was **not**
repaired: it is outside this pass's file ownership. Those bytes are
unrecoverable by decoding; like the `M22` header above they can only be
reconstructed from context.

---

## Deliberately not changed

- **`BENCHMARK.md`** — its numbers are measurement records, not prose claims.
- **`vm/Cargo.toml`** and every other `.rs` file — eight other agents were
  editing Rust sources concurrently; only the two named encoding repairs were
  in scope.
- **The document's voice and structure.** The corrections are surgical. The
  candour was already there and is the reason the errors were findable.
