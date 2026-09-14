# Profiling and debugging JIT code

Compiled Java methods run from anonymous executable memory, which has no symbol
table. Without help, `perf`, `samply` and `gdb` show JIT frames as bare
addresses or `[unknown]`. CratonVM can publish symbols for that memory through
three independent sinks, all **off by default**:

| Flag | Grouped spelling | What it produces | Platforms |
|---|---|---|---|
| `CRATONVM_JIT_PERF_MAP` | `CRATONVM_JIT=perf-map` | `perf-<pid>.map` text symbol map | Linux, macOS, Windows |
| `CRATONVM_JIT_JITDUMP` | `CRATONVM_JIT=jitdump` | `jit-<pid>.dump` for `perf inject --jit` | Linux x86-64 and aarch64 |
| `CRATONVM_JIT_GDB` | `CRATONVM_JIT=gdb` | GDB/LLDB JIT interface registrations | Linux |

Each flag accepts `1`, `true`, `on` or `yes`. The VM reads a flag once, the
first time code is published. Several can be combined:
`CRATONVM_JIT=perf-map,jitdump`.

The implementation is in `jit/src/code_events.rs` (event fan-out and naming),
`jit/src/perf_map.rs`, `jit/src/jitdump.rs` and `jit/src/gdb_jit.rs`.

## What gets named

A record is written when a region becomes callable:

| Region | Name |
|---|---|
| Method body from the single-pass backend | `java/lang/String.hashCode()I [c1]` |
| Method body from the optimizing IR backend | `java/lang/String.hashCode()I [c2]` |
| OSR body | `pkg/Cls.loop([I)J [osr@17]`, where 17 is the bytecode pc it was compiled for (`[osr]` if not recorded) |
| OSR entry trampoline | `osr-trampoline->0x7f... [stub:osr-trampoline]`, where the address is the loop entry it jumps to |
| Lambda adapter thunk | `lambda-adapter->0x7f... [stub:lambda-adapter]`, where the address is the implementation method's entry |

Bodies from the aarch64 backend are labelled `[c1]`. Deopt, bounds-check,
null-check and local-handler stubs live inside the body's own buffer, so
samples in them are attributed to the method.

Names may contain spaces. Line breaks and NUL bytes are removed.

## perf map

```bash
CRATONVM_JIT=perf-map perf record -g -- cratonvm -cp app.jar Main
perf report
```

The map is written to `/tmp/perf-<pid>.map` (Linux and macOS) or
`%TEMP%\perf-<pid>.map` (Windows). The file is truncated when the VM opens it,
then gets one line per region: `<hex start> <hex size> <name>`. Each line is
flushed immediately, so the map is complete even if the process crashes.

Lines are never removed. When freed code's address is reused, the newer line
wins in perf's lookup. A sample taken *before* the reuse can therefore be
misattributed to the newer method. jitdump does not have this problem.

`samply record` also reads `/tmp/perf-<pid>.map`.

Frame-pointer call graphs (`perf record -g`, which is `--call-graph fp`) work
through JIT frames: every compiled frame opens with `push rbp; mov rbp, rsp`.

## jitdump

jitdump carries the machine code itself, not just the address ranges. perf can
then disassemble JIT code (`perf annotate`), and freed-and-reused addresses are
resolved by timestamp.

```bash
# 1. Record. -k 1 selects CLOCK_MONOTONIC, the clock the dump is stamped with.
CRATONVM_JIT=jitdump perf record -k 1 -g -- cratonvm -cp app.jar Main

# 2. Turn the dump into one ELF image per compiled body.
perf inject --jit -i perf.data -o perf.jit.data

# 3. Report on the injected file.
perf report -i perf.jit.data
```

Details:

- **File location.** The file is `$JITDUMPDIR/jit-<pid>.dump`, and
  `JITDUMPDIR` defaults to `/tmp`. `perf inject` writes its
  `jitted-<pid>-<n>.so` files into the same directory, so it must be writable
  and have room for a copy of all JIT code.
- **The mmap marker.** The VM maps the file `PROT_READ | PROT_EXEC` once, which
  is how `perf record` discovers it. A directory on a `noexec` mount refuses
  that mapping. The sink then turns itself off and prints one stderr line. Set
  `JITDUMPDIR` to another directory.
- **Records.** The dump contains the file header, then one `JIT_CODE_LOAD` per
  region with a monotonically increasing code index.
  - On x86-64, a load is preceded by `JIT_CODE_UNWINDING_INFO`, but only when
    the body's first bytes are the standard frame record `55 48 89 E5`. The
    unwind table encodes "CFA = RBP+16, return address at CFA-8, RBP at
    CFA-16" after the prologue. It makes `perf record --call-graph dwarf` work
    through JIT frames.
  - No unwind table is emitted on aarch64.
- **`JIT_CODE_CLOSE`** is written by an `atexit` handler. That handler runs
  when the process exits through `exit` (a normal return from `main`, or
  `System.exit`). A crash or a kill skips it; `perf inject` does not need the
  record.
- **Unloads.** jitdump version 1 has no unload record. perf resolves reused
  addresses by timestamp.

## GDB and LLDB

```bash
CRATONVM_JIT=gdb gdb --args cratonvm -cp app.jar Main
(gdb) run
...
(gdb) bt                 # JIT frames are named
(gdb) info symbol $pc
(gdb) disassemble $pc-32,$pc+32
```

The VM exports `__jit_debug_descriptor` and `__jit_debug_register_code`, and
follows the standard `JIT_REGISTER_FN` / `JIT_UNREGISTER_FN` protocol. GDB
enables its JIT reader automatically when it finds those symbols.

For LLDB, run `settings set plugin.jit-loader.gdb.enable on` first.

Each registration is a minimal in-memory ELF64 object:

- a `.text` section at the code's address;
- one function symbol covering the code;
- no line table and no DWARF unwind data.

GDB unwinds JIT frames through the frame-pointer chain.

Entries are unregistered just before their code is unmapped, so the debugger
never keeps a symbol for a reused address.

The debugger stops the process briefly on every registration. With many
compilations that noticeably slows startup under the debugger. It costs nothing
when no debugger is attached.

Names contain `(`, `/` and spaces, so a breakpoint on one is easiest by
address: `break *0x7f...` taken from `info symbol` or `bt`.

## Windows

- `CRATONVM_JIT_PERF_MAP` writes `%TEMP%\perf-<pid>.map` in the same text
  format. It is for tools and scripts that read that format.
- WPA, VTune and the Visual Studio profiler do **not** read it. CratonVM has no
  ETW or PDB symbol publication for JIT code.
- `CRATONVM_JIT_JITDUMP` and `CRATONVM_JIT_GDB` are Linux-only. Setting them on
  another platform prints one stderr line and does nothing else.

## Limitations

- **No bytecode-level debug info.** There are no `JIT_CODE_DEBUG_INFO` records
  and no DWARF line tables. A sample or a frame names the method, not the
  source line or bci.
- **Inlined callees are not separate frames.** Their time is charged to the
  method they were inlined into.
- **The jitdump code copy is taken at publication.** Inline-cache slots and
  call targets patched later are not reflected in `perf annotate`.
- **The unwind table ignores the epilogue.** A sample on the final
  `pop rbp` / `ret` bytes unwinds one frame wrong.
- **Errors disable the sink.** An I/O error in any sink turns that sink off for
  the rest of the process, with one stderr line starting `[cratonvm]`.
- **This is separate from crash reports.** `CRATONVM_DBG_JIT_NAMES` fills the
  in-process name table used by crash reports. It is unrelated to these flags.
