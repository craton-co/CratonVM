# tools/text-order: pack the boot-executed functions of `cratonvm` together

On Linux every page of the launcher's text that boot touches is resident, and
the kernel maps a 64 KiB window around each first touch. A Hello world executes
~27 000 functions (~13 MB) scattered over the 58 MB `.text`, so ~42 MB of text
was resident. Linked with those functions first (LLD
`--symbol-ordering-file`), resident text is ~15 MB.
Measurements: `docs/feature-designs/real-jdk-mode.md`.

| file | what |
|---|---|
| `boot-text.order` | the template: FNV-1a-64 hashes of the **demangled** names of the boot-executed functions, in first-execution order. Demangled, because the crate hashes inside mangled Rust names change with the build profile (LTO, codegen units) and the toolchain. |
| `relink.sh` | applies the template to an already-built binary: resolves it against the binary's own symbols (`order.py resolve`) and re-runs only the launcher's final crate and link. Release CI runs it after `cargo build --release` on `x86_64-unknown-linux-gnu`. Skips (exit 0) when the binary was not linked by LLD or under 50 % of the template resolves. |
| `census.sh` | regenerates the template on a Linux x86_64 host with a JDK 25: builds a function-per-page binary (`-C llvm-args=-align-all-functions=12`), runs the boot probes under `texttrap.so`, writes `boot-text.order`. |
| `texttrap.c` | the `LD_PRELOAD` census: the executable's text is made `PROT_NONE` before `main` and every first execution of a page is logged and unprotected (foreign SIGSEGVs are chained to the program's own handler). |
| `order.py` | `template` (traps -> template) and `resolve` (template + binary -> ordering file). |

Local use, after a release build on Linux:

```bash
tools/text-order/relink.sh --release
```

A stale template only loses coverage (functions it does not name are linked
where they always were); `relink.sh` prints how much resolves. Regenerate it
when that drops noticeably:

```bash
JAVA_HOME=/usr/lib/jvm/java-25-openjdk-amd64 tools/text-order/census.sh
```
