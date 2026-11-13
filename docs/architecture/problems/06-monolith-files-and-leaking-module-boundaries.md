# Problem 6: half the code is in 92 files, and module boundaries leak

Filed 2026-11-11 from code analysis only. Measured on the tree at `39e1145b06` (`dev` of 2026-11-01). Counts are regex counts over `*.rs`
and are approximate.

## Problem

The workspace is about 4.2 million lines of Rust in 24 crates. The crate
split is by layer name, not by ownership: the largest crates are single
compilation units of a million lines, the largest files hold whole subsystems
with their tests, and the interfaces between crates are bypassed.

## Evidence

- **92 files are over 10,000 lines and hold 2.35 million lines**, more than
  half the code. 213 files are over 5,000 lines. The top of the list:
  `jit/src/ir_lower.rs` 159,723; `jit/src/lib.rs` 98,613;
  `gc/src/gen_heap.rs` 81,075; `vm/src/vm/vm_exec.rs` 80,461;
  `gc/src/g1.rs` 78,003; `jit/src/ir.rs` 68,749; `gc/src/zgc.rs` 66,859;
  `vm/src/runtime/interpreter/jit_bridge.rs` 65,438;
  `native-builtins/src/lib.rs` 62,310.
- Tests live in the same files: `ir_lower.rs` has 955 `#[test]` functions and
  its test module starts at line 105,762; `gen_heap.rs` 665; `g1.rs` 652;
  `zgc.rs` 557. Any test edit recompiles the production code of the crate.
- Crate sizes: `vm` 1,164,579 lines, `native-builtins` 1,150,082, `jit`
  894,452, `gc` 492,280. The release profile is fat LTO with
  `codegen-units = 1` (`Cargo.toml:341`), so a one-line change in `vm`
  rebuilds and relinks all of it.
- The collector interface is not the interface. `GarbageCollector`
  (`gc/src/collector.rs:569`) has 26 methods; `VmHeap`
  (`gc/src/vm_heap.rs:990`) is an enum of the three collectors with about 293
  public inherent methods, and code outside `gc` matches on its variants at
  about 269 sites in `vm`. Collector names appear about 2,000 times in `jit`.
- Each collector is one file. `gen_heap.rs`, `g1.rs` and `zgc.rs` each define
  1,500 to 1,750 functions and share only 41 function names, so common work
  (reference processing hooks, root walks, object walks) is written per
  collector against no shared skeleton.
- Dependency edges that cross layers: `gc -> cuda-bridge`,
  `classloading -> jit-api`, `classloading -> native-builtins-crypto`,
  `native-collections -> gc`, `jit-api -> native-api`. 125 `include!` /
  `#[path]` splits stitch files together outside the module tree.
- 10,749 `unsafe` sites (vm 3,188; gc 2,901; jit 2,134) are spread through
  these files with no narrow module that owns raw object access.

## Why it is architectural

- Parallel work collides: every JIT change touches `ir_lower.rs` or
  `jit/src/lib.rs`, every generational change touches `gen_heap.rs`.
- Tools fail or crawl on files this size (format, IDE indexing, review
  diffs), so review quality drops exactly where the risk is highest.
- A leaked boundary cannot be changed. Adding or removing a collector means
  editing hundreds of match arms in `vm` and `jit`.

## Plan

Do the cheap mechanical steps first. None of them changes behaviour.

1. **Move tests out.** For every file over 10,000 lines, move
   `#[cfg(test)] mod tests` bodies to sibling `tests/` module files. This
   alone removes roughly a third of each monolith and is a pure move.
2. **File size ratchet.** CI fails if any `.rs` file grows past its current
   size when over 10,000 lines, and no new file may exceed 5,000.
3. **Split by the seams that already exist.** `ir_lower.rs` by node family
   (the same families problem 5 uses); `gen_heap.rs` / `g1.rs` / `zgc.rs`
   into allocate, mark, evacuate, references, sizing; `vm_exec.rs` into
   invoke, resolve, class init; `jit/src/lib.rs` into compile driver, code
   registry, tier policy.
4. **Close the collector boundary.** Grow `GarbageCollector` (or a second
   trait for the VM-facing part) until `vm` no longer matches on `VmHeap`
   variants; make the variants private to `gc`. The JIT gets barrier
   emission through one `BarrierSet` description per collector instead of
   collector names.
5. **Split crates along the new files** where it buys build time: collectors
   into `gc-gen`, `gc-g1`, `gc-zgc` over a `gc-core`; `native-builtins` by
   JDK package. Remove the upward edges listed above (`gc -> cuda-bridge`
   becomes a hook the GPU crate registers).
6. **Fence `unsafe`.** Raw object access goes through one small module per
   crate; a lint denies `unsafe` elsewhere in new code.

## Done when

- No file over 10,000 lines; no production file contains its tests.
- `VmHeap`'s variants are not nameable outside `gc`.
- An edit to one collector does not rebuild the other two.
