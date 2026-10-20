# Unloaded classes' mirror names stay in the strong interned-string pool (ZGC non-hidden classes, G1 all)

> **STATUS (2026-10-19, zt t7/cl): residual 1 FIXED IN CODE for the mirror's own name, PENDING BUILD/PROBE; what is left moved to `zgc-zt7cl-define-path-and-class-name-residuals-20261019.md` §5.** Decided by reading: no `==` needs the mirror's name String pooled. The slot holds this VM's INTERNAL name (`pkg/Foo`), an overlay HotSpot does not have: HotSpot's `Class.name` stays null until the first `getName()`, which stores the interned binary name in it. Java reaches identity only through `Class.getName()`, whose native interns its own answer (`lang_class::native_class_get_name`: every arm calls `intern_string`), so `getName() == "Foo"` keeps HotSpot's answer either way. Core reflection filters `Class`'s fields, and the `getName()` memo compares the slot with itself. So on ZGC, `class_mirror_impl` (vm/src/vm/vm_object.rs, `mirror_name_unpooled`) no longer pools the name of a class with a defining-loader row (a loader object defined it: the classes that can unload). A reserved mirror's parked name is reused as is. `CRATONVM_ZGC_CLASS_NAME_UNPOOLED`: default ON, `=0` kill, read once per process; G1 and Generational unchanged. Rows: unit test `zt7cl_a_loader_defined_class_mirror_name_stays_out_of_the_pool_on_zgc`; probe `Zt7ClClassNames` (stdout SAME as `java -XX:+UseZGC`; stderr `growth_bytes_per_define` near 0, against ~84 on the `=0` arm). Still open (§5 of the new page): a class whose `getName()` was called keeps its interned binary name in the strong pool (that closes with this page's weak-table flip), and the `getName()` memo's global root (`CRATONVM_CLASS_GET_NAME_MEMO`) is never released on unload, on every collector. Residual 3's row is unchanged. Report: `docs/internal/zgc-round-zt-20261018/t7-cl-report.md`.**

> **STATUS (2026-10-19, zt t6/nd4): OPEN, flip decision written; no code change.** Residual 1: the default ZGC is stop-the-world only (`CRATONVM_ZGC_CONC_START` defaults to 0, generational off), so `VmHeap::zgc_weak_string_table_sound` is true there and flipping `CRATONVM_ZGC_WEAK_STRING_TABLE` engages the existing STW sweep with no new code. The concurrent keep-alive is NOT a prerequisite of the flip: with concurrent or generational ZGC configured the predicate is false and the pool stays strong (sound). The flip needs: z1-rf rows I1-I8 SAME (`docs/internal/zgc-round-zr-20261012/z1-rf-report.md`), the soundness gate (20 runs each of I2 and `MtChurnProbe` with `=1`, no `Stale pointer` / corrupt-object line), the nd4 `N3W` arm extended to a non-hidden user loader that dies, an interleaved CPU A/B on an intern-heavy row (<= 1.05x), the read switched to `runtime_flag_default_on` (vm/src/memory/string_table.rs, `set_zgc_weak` reads `runtime_flag_on` today) with any test that pins the default updated. Residual 2 is G1's (not ZGC's, nothing to fix here). Residual 3: the CVM-only row `Zs2Nd2FfmMints define 128 20000` at `-Xmx64m` (`door_oome=0 check=20000`). Report: `docs/internal/zgc-round-zt-20261018/t6-nd4-report.md`.**

> **STATUS (2026-10-19, zt t5/pg3): OPEN (residuals 1 and 3).** Hidden classes FIXED and host-confirmed. Open: 1, non-hidden unloaded names stay pooled: flip `CRATONVM_ZGC_WEAK_STRING_TABLE` (vm/src/memory/string_table.rs, opt-in) after its rows, with a keep-alive on lookup while a concurrent mark is open; 3, the local row `cratonvm --enable-native-access=ALL-UNNAMED -XX:+UseZGC -Xmx64m Zs2Nd2FfmMints define 128 20000` (pass `door_oome=0 check=20000`). Residual 2 is G1's (the `gcg-g2w` weak-string-table proposal).

> **STATUS (2026-10-19, zt t1/nd): unchanged. Residual 1 (non-hidden names pooled; the reserved mirror pools them at the adoption) needs the ZGC weak string table's flip (`CRATONVM_ZGC_WEAK_STRING_TABLE`, opt-in; not this lane's code). Residual 3's row, CVM only (HotSpot dies in its own fill at this margin): `cratonvm --enable-native-access=ALL-UNNAMED -XX:+UseZGC -Xmx64m Zs2Nd2FfmMints define 128 20000`, pass `door_oome=0 check=20000`. Residual 2 is G1. Report: `docs/internal/zgc-round-zt-20261018/t1-nd-report.md`.**

> **STATUS (2026-10-16, zs s10/pg2): OPEN -- the hidden-class part is host-confirmed on s4a (zs orch-notes: `nd2_N3_*` DIFF -> SAME, nd4 N3 / N3W / N3Rm SAME, kill arm N3K keeps the leak at `door_oome` 6287; `CRATONVM_ZGC_HIDDEN_NAME_UNPOOLED`, default ON). Residual 1 is open: `class_mirror_impl` still pools non-hidden names, and `CRATONVM_ZGC_WEAK_STRING_TABLE` is still opt-in (`runtime_flag_on`, vm/src/memory/string_table.rs). Residual 3 is open: the 20 000-define `-Xmx64m` row has not run. Residual 2 is G1, out of scope.**

> **STATUS 2026-10-15 (zs s5/pg): PARTLY -- The hidden-class part is fixed in code (5b3a274d06, `CRATONVM_ZGC_HIDDEN_NAME_UNPOOLED`, ON; s4a rows `nd4_N3*` queued, no host result). Residuals 1-3 are open: `class_mirror_impl` (vm/src/vm/vm_object.rs:1907) still pools non-hidden names, `CRATONVM_ZGC_WEAK_STRING_TABLE` is still opt-in, and G1 has no weak table.**

> **STATUS (2026-10-15, zs s4/nd4): OPEN (residual).** Hidden classes on ZGC are fixed:
> `CRATONVM_ZGC_HIDDEN_NAME_UNPOOLED` (default ON, `=0` kill) keeps their names out of the pool.
> This page is what that switch does not cover.

## The defect

`class_mirror_impl` (`vm/src/vm/vm_object.rs`) writes every mirror's `name` field through
`create_java_string` / `try_create_java_string`. Both insert the String into
`shared.mem.string_pool`.

That pool is a strong root (`roots::collect_roots` section 5) except where the weak string table
engages. The weak table is default ON on Generational only. On ZGC it is opt-in
(`CRATONVM_ZGC_WEAK_STRING_TABLE`, STW-only ZGC); on G1 it does not exist
(`gcg-g2w-proposal-weak-string-table-on-g1-and-zgc-20261001.md`).

So a class that unloads (classes are unloaded on all three collectors:
`memory::gc::unload_dead_class_metadata`) leaves its name String behind for the life of the VM.
HotSpot's `StringTable` is weak, so there the interned name (`java_lang_Class::name` interns it)
dies with the mirror.

## Measured (zs s4/nd4, local, debug)

Probe: a `Lookup.defineHiddenClass` loop, then `System.gc()` every 250 defines; the counter is
the used heap after the collection. Each define is followed by a 512-byte garbage array.

| arm | used after GC (KB) at 250 / 500 / 750 defines |
|---|---|
| `-XX:+UseZGC` (ZGC), `CRATONVM_ZGC_HIDDEN_NAME_UNPOOLED=0` | 31741 / 31762 / 31784 (jdk-only); 31720 / 31741 / 31760 (`--compatible`) |
| ZGC, default (switch on) | 31717 / 31719 / 31720; 31696 / 31698 / 31698 |
| ZGC, `=0` + `CRATONVM_ZGC_WEAK_STRING_TABLE=1` | 31717 / 31719 / 31720 (`[string-table] dropped=250` per cycle: exactly one pooled name per define) |

The leak is ~84 bytes per define. On `Zs2Nd2FfmMints define 1024 20000` the 1 MiB that the
probe leaves free was gone after ~13 600 defines. Every later iteration then threw, so the host
saw `door_oome` ~6400 against HotSpot's 0. No native door refused, and `native_replays=0`: the
failing allocation was the probe's own `new byte[512]`.

## Still open

1. **ZGC, non-hidden unloadable classes.** These are classes of a user loader that dies (web-app
   redeploys, a per-test class loader). Their binary names can equal a String the program
   interned, so dropping them from the pool is not invisible the way a hidden class's minted
   name is. The clean fix is the weak table on ZGC. It is already built for STW-only ZGC
   (`CRATONVM_ZGC_WEAK_STRING_TABLE`); it needs its flip rows and the concurrent-mark arm
   (keep-alive on lookup while marking).
2. **G1.** The same leak for hidden classes and for unloaded classes. Out of this round's scope.
   It closes with the weak table on G1 (the proposal above).
3. **A smaller residual on ZGC.** With the switch on, the used heap still grows by ~1-3 KB per
   500 defines (about 4 bytes per define; within GC-state noise at this resolution). A 20 000-define
   run at `-Xmx64m` with a 128 KiB margin would show it if it is real.
