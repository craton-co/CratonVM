# psample — a RIP sampler for a running `cratonvm` process (Windows)

`ETW`/`wpr` needs an elevated shell. This does not: it opens the target with
`PROCESS_QUERY_INFORMATION | PROCESS_VM_READ`, then every `<interval_ms>` it
suspends each thread, reads `RIP`, and resumes it. Native frames are named
through `dbghelp` and the build's PDB (`target/release/cratonvm.pdb`); JIT code
(no module) is named from the `perf-<pid>.map` the VM writes into `%TEMP%`
when run with `CRATONVM_JIT_PERF_MAP=1`.

```
cargo build --release --manifest-path tools/psample/Cargo.toml
$env:CRATONVM_JIT_PERF_MAP = '1'        # in the shell that starts the VM
tools\psample\target\release\psample.exe <pid> 1 30 <dir with cratonvm.pdb> | Select-String -NotMatch 'ntdll!'
```

Output is exclusive samples per symbol (top 60). Wait states (`ntdll!Zw*`) are
parked threads — filter them. It is a flat profile, not a call tree, and
suspension makes it bias toward long instructions; use it to find WHICH of
dispatch / interpreter / C1 code / C2 code the time is in, then read that code.

Used for `docs/known-issues/tomcat/jit-native-call-crossing-250ns-threadlocal-get-2us-20261005.md`.

## Call chains

`PSAMPLE_STACK=<depth>` also walks each sampled thread's native stack (x64
unwind data through dbghelp and the PDB; the walk stops at the first JIT
frame, which has no unwind data), and `PSAMPLE_FILTER=<substring>` keeps only
the stacks whose TOP symbol contains it (empty: all). The report ends with the
25 most common chains, top frame first:

```
$env:PSAMPLE_STACK = '12'; $env:PSAMPLE_FILTER = 'cratonvm'
tools\psample\target\release\psample.exe <pid> 2 45 <dir with cratonvm.pdb>
```

Used for `docs/internal/fixed-suite-bugs/tomcat/52-initiating-memo-rewrite-bumped-the-resolution-epoch-on-every-loadclass-drive-multicast-burst-lost-half-FIXED-20261006.md`.
