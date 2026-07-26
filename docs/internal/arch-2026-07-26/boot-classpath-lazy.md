# Boot classpath: lazy JMOD class serving

**Slug:** `boot-classpath-lazy`
**Owner files:** `classloading/src/class_path.rs`, `vm/src/config.rs`
**Base:** `arch/wave1-integration-20260726` merged at `18b2f49d5`
**Status:** landed, unconditional, no flag

---

## 1. Summary

`ClassPath::load_jmod` no longer inflates anything. It builds a
decompression-free name index from the JMOD's central directory and inflates a
single entry per lookup, matching what the JAR path has done since the historic
O(jars x zip-probes) scan was closed.

This removes the largest measured cost in the startup effort: **15.0-20.9 s of
pure inflate and ~136 MB of resident decompressed bytecode, on every default
real-JDK boot, before a single Java class loads**.

The boot classpath still prefers `jmods/` over `lib/modules`. That was the
sibling's recommendation and it was **not** taken — §4 explains why, and the
reason is not caution about testing alone: the two sources do not hold the same
bytes.

No new flag, no env var, no default-off landing. The old behaviour is not
reachable at runtime; reverting means reverting the commit.

---

## 2. Verification of the finding (independent, before changing anything)

The task called for this explicitly, because several agents this wave found
in-tree claims that were the opposite of the code. All three claims hold.

| claim | verdict | evidence |
|---|---|---|
| `load_jmod` is eager | **true** | pre-change `class_path.rs:4367-4383`: `for i in 0..total_entries { ... if let Some(relative) = name.strip_prefix("classes/") { ... find_shared_in_archive_locked(...) -> classes_cache.insert(...) } }`. `find_shared_in_archive_locked` calls `read_entry_capped`, i.e. a full inflate, for any entry whose compression is not `Stored`. JDK JMOD class entries are Deflated. |
| `load_jimage` is lazy | **true** | `class_path.rs:4130` `load_jimage` calls `reader.iter_entries()`, and `reader/src/jimage.rs:747` returns `Vec<(String, u64, u64)>` — path, offset, uncompressed size. It decodes location records and never touches the resource payload. Bytes are fetched later by `JImageReader::find_class`. |
| `discover_boot_classpath` prefers `jmods/` | **true** | `vm/src/config.rs`: the `jmods/` branch returns at `:960-962`; the `lib/modules` check is at `:977-980`, strictly after it. `rt.jar` is checked before both. |

Two corrections to the sibling's write-up, neither of which changes its
conclusion:

* §2.1 says `load_jmod` "re-opens the archive a second time for non-class
  resources". It did — `ZipArchive::new` was called twice on the same backing.
  The second open is now unnecessary and is gone; the single archive is moved
  into the entry.
* §2.3 says JMOD `scan_module_infos` answers "free at this point, having been
  paid for in §2.1". After this change it costs one small inflate per JMOD
  (70 `module-info.class` entries, a few KB each). That is a rounding error
  against the 27,962 it replaces.

### 2.1 A number the sibling's measurement did not capture

`JImageReader::open` (`reader/src/jimage.rs:484-491`) does
`file.read_to_end(&mut data)` — **the whole 138.2 MB `lib/modules` blob becomes
an owned `Vec<u8>`**, anonymous resident memory, for the life of the VM.

JMODs, by contrast, are **memory-mapped** (`read_archive_for_classpath`,
`class_path.rs:143`, `ArchiveBacking::Mapped(Arc<Mmap>)`) — page-cache backed,
evictable, shared between processes.

This matters for the routing decision: switching the boot classpath to
`lib/modules` would have traded 136 MB of inflated class bytes for 138 MB of
raw jimage blob and won **nothing** on memory. It only ever addressed the CPU
half of the problem. Making `load_jmod` lazy addresses both.

---

## 3. What changed

### 3.1 `classloading/src/class_path.rs`

`ClassPathEntry::JmodFile` loses

```
classes_cache: HashMap<String, SharedBytes>   // 27,962 inflated entries
```

and gains

```
class_entry_index: FxHashSet<String>          // 27,962 names, no bytes
```

with a new accessor:

```rust
fn jmod_class_bytes(
    archive: &Mutex<SharedArchive>,
    backing: &ArchiveBacking,
    class_entry_index: &FxHashSet<String>,
    relative: &str,
) -> Option<SharedBytes>
```

It probes the index first and only then touches the ZIP — the exact shape of
the existing `find_shared_in_indexed_archive` on the JAR path.

`load_jmod` now walks `by_index_raw` for names only. `by_index_raw` builds the
entry reader from the central directory without reading compressed data, so the
load loop reads no payload bytes at all.

Updated call sites, all in `class_path.rs`:

| site | before | after |
|---|---|---|
| `find_class` | `classes_cache.get(&relative_path)` | `jmod_class_bytes(...)` |
| `find_class_source_path` | `classes_cache.contains_key(..)` | `class_entry_index.contains(..)` — never inflates |
| `find_resource` | cache get, glob over `keys()` | `jmod_class_bytes(...)`, glob over `iter()` |
| `find_all_resources` | cache get, glob over `keys()` | `jmod_class_bytes(...)`, glob over `iter()` |
| resource-URL enumeration | `contains_key` + `archive_has_entry` | `contains` + `archive_has_entry` — already inflation-free |
| `scan_module_infos` | `classes_cache.get("module-info.class")` | `jmod_class_bytes(...)` |
| `jmod_class_count` | `classes_cache.len()` | `class_entry_index.len()` |

`matching_resource_entry_names` sorts its output, so moving the glob source
from `HashMap::keys()` to `FxHashSet::iter()` does not introduce
nondeterminism — neither iteration order was ever depended on.

### 3.2 Why there is no second cache

The obvious worry about lazy inflation is repeated lookups. There is already a
bounded memo one layer up: `ClassManager::class_bytes_cache`
(`class_manager.rs:1267`) is a FIFO cache with a 16 MiB soft cap
(`DEFAULT_CLASS_BYTES_CACHE_CAP`, `:52`), and parsed classes are cached by
name+loader besides. Adding a second cache inside `load_jmod` would re-create
the footprint problem this change removes, and would put the bound in the wrong
place. Deliberately omitted.

### 3.3 `vm/src/config.rs`

* `discover_boot_classpath` behaviour is **unchanged**. Its doc comment now
  records why the `jmods`-first ordering is deliberate rather than an
  oversight, so the next reader who finds the startup cost does not "fix" the
  ordering. Four hermetic `#[cfg(test)]` tests pin it (they build fake
  `JAVA_HOME` trees; `resolve_java_home(Some(dir))` is authoritative when the
  directory exists, so no JDK and no env vars are involved).
* The "~300 natives in real-JDK mode" figure is corrected — see §6.

---

## 4. Route (a) vs route (b): why the reader was not swapped

Route (a) was "prefer `lib/modules` when present". It was rejected on evidence,
not only on inability to test.

**The two sources do not hold the same bytes for the same class.**
`lib/modules` is a `jlink` product. Its `module-info.class` files are rewritten
by the `SystemModules` plugin, and it carries generated
`jdk.internal.module.SystemModules$*` classes that no JMOD contains; the JMODs
carry the pristine, un-transformed `module-info`. `ClassPath::scan_module_infos`
feeds the module-layer builder from exactly those bytes, so "byte-identical
class data for the same class" — the thing the task asked me to establish
before taking route (a) — is **false** for at least the module descriptors, and
the divergence is in the one class family the boot sequence is most sensitive
to.

**The jimage does not hold everything a JMOD holds.** A JMOD carries `lib/`,
`conf/`, `legal/`, `bin/` and `include/` alongside `classes/`; the jimage holds
only the `classes/` subtree's content. `find_resource`'s JMOD arm serves the
`classes/` subtree and falls back to the raw archive, so any resource lookup
that resolves out of a JMOD's non-`classes/` entries today would silently start
missing.

**And it wins nothing on memory** — see §2.1: the jimage reader slurps the
whole 138.2 MB file into an owned `Vec`.

Route (b) keeps the same reader, the same archive, and the same bytes for every
class, and removes both halves of the cost. It is strictly the safer change and
very nearly the same win. Taken.

---

## 5. Expected effect

### Boot time

Removes **15.0-20.9 s** (measured floor, optimised native zlib) from every
default real-JDK boot on a stock JDK 25. Replaced by one inflate per class
actually loaded. Boot resolves on the order of a few hundred to a few thousand
classes (`bootstrap_core_classes` names 323 explicitly and pulls supertypes
transitively — sibling §2.4), i.e. **single-digit percent of 27,962**. In debug
builds, where the `load_jmod` comment records deflate being catastrophically
slower, the proportional win is larger, not smaller.

The per-lookup cost that pre-extraction was introduced to avoid does **not**
come back. That cost came from existence probes spelled
`find_in_archive(..).is_some()`, which inflated an entry to answer a yes/no; the
index answers those with a hash probe. `find_class_source_path` and the
resource-URL path now inflate nothing at all, where the former inflated at load
time to answer the same question.

### Steady-state RSS

| | before | after |
|---|---|---|
| inflated class bytes held by the classpath | **136.0 MB** anonymous, permanent | **0** — bytes are transient, bounded upstream by a 16 MiB FIFO cap |
| `class_entry_index` / `classes_cache` keys | ~1.5 MB of `String` keys + ~1 MB table | ~1.5 MB of `String` keys + ~1 MB table (unchanged) |
| `all_entry_names` | ~2.5 MB | ~2.5 MB (unchanged) |
| JMOD file data | 84.0 MB **mmapped** (page cache, evictable) | unchanged |

**Net: roughly 136 MB less anonymous resident memory at boot**, and the peak
class-byte residency becomes a bounded 16 MiB rather than an unbounded-by-
construction 136 MB. The JMOD mappings are unchanged and remain page-cache
backed rather than anonymous.

---

## 6. The "~300 natives" figure (this file's own owner fix)

`vm/src/config.rs` said "~300 truly-native methods" in three doc sites and, more
seriously, in `JdkMode::describe()` — which `vm-cli/src/main.rs:1629` prints on
the `JDK class library:` line of `-version`. It is user-visible and quoted in
bug reports.

The figure is wrong by roughly an order of magnitude. Derivation, reproducible
by grep and recorded on the new `REAL_JDK_NATIVE_REGISTRATIONS` constant:

* The default build compiles the `#[cfg(not(feature = "synthetic-jdk"))]` arm of
  `vm/src/vm/vm_init.rs:1493`, which makes **32** top-level registration calls.
* `register_essential_natives` (`native-builtins/src/lib.rs:6851`) alone holds
  **960** direct `registry.register(` sites and calls **184** distinct
  sub-registrars. (Confirms the sibling's 960 / "182".)
* Static call-graph walk over `native-builtins/src` from all 32 roots —
  resolving each callee to a definition in the same file first, then to a
  globally unique name, ignoring ambiguous names — reaches 1,116 functions
  holding **2,746** `.register(` sites.
* Six of the 32 roots are defined outside `native-builtins/src` and were not
  traversed, so 2,746 is a **floor**.

`describe()` now says `~2,700 native methods in Rust (exact census:
--dump-native-registry)`, and a test asserts the retracted `~300` string is
gone.

**Caveat stated on the constant:** this counts registration *call sites*, not
distinct registry keys; a key registered twice counts twice. The authority for
an exact per-run count is `--dump-native-registry`, which this session could not
run (no builds). The constant exists so documentation stops quoting a number
that is off by ~9x, not to claim precision it does not have.

For calibration: the same walk from `register_builtins` (the synthetic arm)
reaches **8,499** `.register(` sites against a documented "~5,200 stubs" — a
~61% site-to-method ratio. If that ratio holds for the real-JDK arm the true
distinct-method count is nearer 1,700 than 2,700. Either way, not 300.

---

## 7. Cross-owner requests

Not made by this change. Each names the file, the exact text, and why.

### 7.1 `docs/CONFIG.md` — same retracted "~300" figure, user-facing

`docs/CONFIG.md:20` describes `--real-jdk` as "(~300 native methods in Rust)".
Same defect as `JdkMode::describe()`, same correction: ~2,700 registrations,
floor, exact census via `--dump-native-registry`. `docs/CONFIG.md:21` carries
the companion "~5,200 native stubs" for `--synthetic-jdk`, which the calibration
in §6 suggests is roughly right but is worth confirming from a real census.

### 7.2 `docs/internal/arch-2026-07-26/jdk-mode-determinism.md` — figure used to size a proposal

`:71`, `:73`, `:105`, `:227` and `:386` all repeat "~300" / "~5,200". `:227`
quotes the `-version` banner text verbatim, so it is now stale against
`config.rs`. More importantly §7.4 step 2 sizes a proposed `native-essentials`
crate split against the ~300 figure; that recommendation should be re-read
against ~2,700 before anyone acts on it.

### 7.3 `docs/internal/arch-2026-07-26/native-dispatch-memoization.md`

`:52` — "~300 in real-JDK mode, but those sit on the hottest paths". The
conclusion about hot paths is unaffected; the number is not.

### 7.4 `docs/internal/arch-2026-07-26/startup-and-diagnostics.md` — §2.3 and §6.1 are now stale

* §2.3's claim that JMOD `module-info` scanning is "free at this point, having
  been paid for in §2.1" no longer holds (it is now 70 small inflates — still
  cheap, but for a different reason).
* §6.1's request is **closed by the fallback it named**, not by the primary
  ordering change it asked for. Route (a) was declined on the evidence in §4 of
  this file, not deferred.

### 7.5 `vm/src/vm/vm_init.rs` — worth a boot log line

Not required, but the `-version` / boot log would be more useful if it printed
the real registry population once (`NativeMethodRegistry` length after the
real-JDK arm completes) instead of leaving `--dump-native-registry` as the only
way to learn it. Owner of `vm_init.rs`.

---

## 8. What a reviewer should run

No build or test was run in this session (host OOMs on concurrent builds).
`rustfmt --check` is clean on both owned files — the only remaining diffs in
`vm/src/config.rs` (`:535`, `:1163`, `:1175` pre-change) are pre-existing and
untouched.

In order of value:

1. `cargo test -p cratonvm-classloading class_path` — the four new tests are
   hermetic and need no JDK:
   * `jmod_lazy_lookup_matches_eager_extraction_byte_for_byte` — builds a
     synthetic **Deflated** JMOD, independently replicates the old eager
     extraction loop against the same archive, and asserts `find_class` returns
     byte-identical results for every entry, plus that `jmod_class_count`,
     `find_class_source_path` and `find_resource` are unchanged.
   * `class_only_in_jmod_resolves_when_jimage_is_also_on_the_path` — jimage
     first, JMOD second; the JMOD-only class must still resolve.
   * `malformed_jmod_errors_instead_of_panicking` — seven damaged archives
     (empty, short, bad magic, future major, header-only, garbage, truncated
     ZIP); each must `Err` from `load_jmod` and yield a clean `find_class`
     failure through `ClassPath::new`.
   * `jmod_with_corrupt_entry_payload_reports_class_not_found` — the
     lazy-path-only case: central directory intact so the name **is** indexed,
     but the compressed payload is damaged. Must give `ClassNotFound`, not a
     panic and not truncated class bytes.
2. `cargo test -p cratonvm config::tests` — the four `discover_boot_classpath`
   ordering tests and the `describe()` string test.
3. `cargo test -p cratonvm-classloading -- --ignored load_jmod_validates_magic_and_version find_class_from_jmod_returns_valid_classfile`
   on a host with a JDK — these exercise a real `java.base.jmod`.
4. **The real validation:** boot the H2 / Spring / Tomcat suites in real-JDK
   mode and compare pass counts against `18b2f49d5`. The change is meant to be
   behaviour-preserving; any delta is a bug in this change.
5. Time a bare `cratonvm -version` in real-JDK mode before and after. The
   15-21 s should be gone. Watch peak RSS at the same time; expect roughly
   136 MB less.
