# `SIGSEGV addr=0x0` in class-mirror slot resolution under sustained heap pressure

## Status
**OPEN** — reproduced 2026-07-31 on **unmodified `dev`**. Not caused by, but
surfaced during, `fix/xmx-heap-budget-20260731`.

## Severity
**MEDIUM** — a hard VM crash (no Java exception, no usable report), but it needs
many minutes of sustained near-exhaustion GC pressure plus class-loading churn;
short near-OOM workloads do not reach it.

## Symptom

```
# A fatal error has been detected by the CratonVM Runtime Environment:
#  SIGSEGV at pc=0x5ea0f75bc83a, addr=0x0, ...
#  r10=0xfe651ba6cbca2334 r11=0x35e4d4822287141b rsp=0x7604167eb600 rbp=0x0
```

`addr=0x0` and `rbp=0x0`, with an `r10` whose top bits are the same
`0xfe651ba6cbca23..` pattern on every occurrence. `--nojit`, so no compiled
frame is involved.

## Reproduce (baseline, no patches)

```bash
cd <h2 checkout>
<dev-tip cratonvm> --java-home <jdk25> --nojit --Xmx 512m \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.db.TestOutOfMemory
```

Crashed at 12:37 into the run (`grows=4`, peak RSS 2.36 GB). Roughly one run in
two to three; the rest either complete with the class's ordinary assertion
failure or exceed a 900 s timeout on a loaded host.

## Why `--Xmx 512m` on baseline, and why this is not the budget fix

The same crash, with a byte-identical register signature, occurred on the
**budget-fixed** build at `--Xmx 1g` (2 of 4 long runs). That build commits a
1 GiB heap where the baseline grew to ~2 GiB, so it reaches comparable GC
pressure at twice the stated heap. Running the **unmodified baseline** at
`--Xmx 512m` — matching that pressure — reproduces it, which is what separates
"the clamp caused this" from "the clamp made an existing crash reachable at a
larger `-Xmx`":

| build | `-Xmx` | long runs | SIGSEGV |
|---|---|---|---|
| dev tip (baseline) | 1g | 3 | 0 |
| dev tip (baseline) | **512m** | 2 | **1** |
| + heap budget | 1g | 4 | 2 |

Register signature and faulting-symbol attribution are identical across
baseline and patched builds.

## Attribution (weak — treat as a lead, not a location)

`addr2line` on the release binary (which carries line tables) maps both faults
into `cratonvm_vm::vm::vm_object::resolve_class_mirror_slots`
(file offsets `0x142983a` baseline / `0x1429a0a` patched — the same function
region). But the build is `lto = "fat"`, `codegen-units = 1`, and addr2line
returned **no file:line** (`cratonvm.<hash>-cgu.0:?`), so the symbol may be a
neighbouring/merged function. See
`docs/internal/fixed-suite-bugs/...crash-report-frames-lie...` for why a single
symbolized address is not a location.

If the symbol is right, the fault is the first dereference in that function
(`class.is_synthetic_stub`) on a `&Class` that both call sites obtain safely via
`class_manager.read().get_class(id)` — a `Vec<Option<Class>>` index behind a
read guard, which cannot be null in safe Rust. That points at the *reference*
being dangling rather than null, i.e. a class-store lifetime problem
(unloading / redefinition under a live borrow), which sustained GC pressure
would make far more likely. Related family:
`reference_loader_granular_unload_shared_proxy_ids`.

## Does not reproduce

The `NativeOomProbe` near-OOM stress (fill the heap with `byte[]`, then with
`ByteBuffer`s, recovering between) run 12x per build at `--Xmx 1g`: 24/24 clean.
Heap exhaustion alone is not enough — the H2 workload's class-loading and
mirror churn appear to be required.

## Next steps

1. Re-run the `--Xmx 512m` baseline repro under `gdb` (passwordless `sudo
   apt-get install gdb` works on the Azure host) for a real backtrace rather
   than a single symbolized pc.
2. If it is `resolve_class_mirror_slots`: audit every `ClassStore` borrow that
   outlives a possible `unload_user_classes_inner` / redefinition, and add a
   debug tripwire on `get_class` returning a slot whose generation moved.
