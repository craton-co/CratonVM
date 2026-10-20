# gcj j2/ob2: launcher and serviceability review residuals (negative `-XX:` values, jcmd texts, PrintFlagsFinal edges)

> **STATUS (2026-10-19, gcj j2/ob2, rows on j2a): items 1, 3, 6 and the `VM.command_line` half of 5 FIXED IN CODE (default ON on Generational, PENDING BUILD/PROBE); items 4, 7 OPEN with HotSpot's text now captured; 2, 8, 9 unchanged.** Captures: `C:/craton/gcj-runs/j2a/vj2-ob2/ob2_cap_*.hs` / `.hse` (Temurin 25.0.3).
> - **Item 1:** HotSpot answers `intx SoftRefLRUPolicyMSPerMB=-1 is outside the allowed range [ 0 ... 9223372036854775807 ]` and `int MaxJavaStackTraceDepth=-1 is outside the allowed range [ 0 ... 1073741823 ]`, each followed by the `Improperly specified VM option` line. `CRATONVM_GEN_HOTSPOT_VALUE_RANGES` refuses with those lines before clap (`vm-cli` `GEN_HOTSPOT_VALUE_RANGES`).
> - **Item 3:** `-XX:MaxDirectMemorySize=0` runs silently on HotSpot (`ob2_cap_mdm0`): now a zero cap. `-XX:MaxMetaspaceSize=0` prints `Error occurred during initialization of VM` / `OutOfMemoryError: Metaspace` on stdout (`ob2_cap_mms0`): now the same (same switch).
> - **Item 4 (`VM.flags`):** HotSpot prints one line, each non-default flag followed by a blank (`-XX:-AOTInvokeDynamicLinking ... -XX:CICompilerCount=15 -XX:InitialHeapSize=268435456 ... -XX:+UseSerialGC -XX:X86ICacheSync=3 `). It names host and JIT ergonomics (`CICompilerCount`, code-heap sizes, `X86ICacheSync`, AOT flags) this VM does not have, so not built: the proposal `gcj-j2ob2-proposal-one-flag-registry-with-origins-20261019.md` is the route.
> - **Items 5 and 6:** `VM.command_line` is `VM Arguments:` / `jvm_args: -XX:+UseSerialGC -Xmx256m -Dob2=x ` (trailing blank) / `java_command: <class> <args>` / `java_class_path (initial): <cp>` / `Launcher Type: SUN_STANDARD`; `VM.uptime` is `0.210 s`. Both built behind `CRATONVM_GEN_JCMD_HOTSPOT_TEXTS` (default ON, Generational).
> - **Item 7 (`VM.symboltable`):** HotSpot prints the statistics block (`Number of entries : 7292`, `avg  47.000`, an integer average) followed by `Shared Symbol Table statistics:` (CDS). Not built (no symbol table to count, no CDS archive).

> **STATUS (2026-10-19, gcj j2/ob2): OPEN.** Filed by lane ob2 of wave j2 from its review of `vm-cli/src/main.rs` (flag parsing) and `vm/src/runtime/serviceability.rs` (jcmd). No build was done. Each item that needs HotSpot's text has a RECORD row in `docs/internal/gc-round-gcj-20261019/j2-ob2-report.md` (block CP). No HotSpot text below is invented: "by reading" marks a claim taken from the JDK source, not from a row. Lines are on branch `gcj2-ob2`.

## 1. A negative `intx` value reaches clap as a separate token

`vm-cli/src/main.rs:4534-4541` (`normalize_java_launcher_argv`) rewrites `-XX:MaxJavaStackTraceDepth=<v>` and `-XX:SoftRefLRUPolicyMSPerMB=<v>` into two tokens, `--XX:<Name>` and `<v>`. For `<v>` = `-1`, clap reads the second token as a short flag. zt t7/x4 observed exactly this for `ZAllocationSpikeTolerance` (`docs/internal/zgc-round-zt-20261018/t7-x4-report.md` finding 2c). The launch therefore ends with clap's usage error and exit status 2, on every collector.

The j1 typed scan accepts `-1` for both, as `intx` values (unit `gcj_j1_ob_gen_value_flag_refusal_lines`), so the early scan does not catch it.

By reading, HotSpot 25 refuses both with a range line: `range(0, max_jint/2)` and `range(0, max_intx)`. That line has no local row.

**Close:**
- capture rows `ob2_cap_sr_neg` and `ob2_cap_mjstd_neg`;
- add the two ranges to `GEN_HOTSPOT_VALUE_FLAGS`, which then refuses with HotSpot's lines before clap;
- for the other collectors, a single `--XX:<Name>=<v>` token, as t7/x4 did.

## 2. `-XX:ZUncommitDelay=0x10` is a clap error

`vm-cli/src/main.rs:2875` types the clap field as `Option<u64>`, so `0x10` or `1k` fails clap's own parse. The j1 typed scan accepts both, as `uintx`.

On Generational, the opt-in `CRATONVM_GEN_ONE_ORDERED_VALUE_PARSE` drops the token. On ZGC, the owner's answer is open: a `String` field plus `hotspot_parse_uintx`.

## 3. An explicit zero `MaxDirectMemorySize` / `MaxMetaspaceSize` warns and is dropped

`parse_size` (`vm-cli/src/main.rs:14281`) refuses 0. Its consumers then print `Warning: ignoring -XX:MaxDirectMemorySize=0 (expected a byte size)` and keep the default.

By reading, HotSpot takes the value. For `MaxDirectMemorySize`, `jdk.internal.misc.VM.saveProperties` reads `sun.nio.MaxDirectMemorySize=0` as a zero cap, not as "use `-Xmx`".

**Close:** rows `ob2_cap_mdm0` and `ob2_cap_mms0`, then the zero arms.

## 4. `jcmd VM.flags` is not HotSpot's

`vm/src/vm/vm_init.rs:18290` (`SharedVm::vm_flags`), printed one flag per line by `vm/src/runtime/serviceability.rs:3978`, has two problems:
- it lists `-XX:+UseGenerationalGC`, `-XX:MaxStackDepth=<n>` and `-XX:CompatibilityMode=<mode>`, which are not HotSpot flags (HotSpot's answer on Serial names `-XX:+UseSerialGC`);
- it lists `-XX:+TieredCompilation` unconditionally.

By reading `JVMFlag::printSetFlags`, HotSpot prints every flag whose origin is not `{default}`, as `-XX:<Name>=<value>` or `-XX:±<Name>`, sorted, separated by blanks on one line.

**Close:**
- RECORD row `ob2_cap_vmflags`, through the DiagnosticCommand MBean's `vmFlags`;
- then the non-default rows of the PrintFlagsFinal table (`gen_print_flags_final_lines`) on Generational. One flag registry serves both (proposal `gcj-j2ob2-proposal-one-flag-registry-with-origins-20261019.md`).

## 5. `jcmd VM.command_line` is invented

`vm/src/vm/vm_init.rs:18268` (`SharedVm::command_line`) answers `cratonvm -Xmx<n>m -cp <cp> -D<k>=<v>...`, rebuilt from the configuration.

By reading `Arguments::print_on`, HotSpot prints `VM Arguments:`, then:
- `jvm_args: ...`;
- `java_command: ...`;
- `java_class_path (initial): ...`;
- `Launcher Type: SUN_STANDARD`.

`VmConfig::jvm_args` already holds HotSpot's `jvm_args` (interpreter round i4 wave 84).

**Close:** RECORD row `ob2_cap_vmcmd`, then the four lines from `jvm_args`, the main class and its arguments, and the class path.

## 6. `jcmd VM.uptime` wording

`vm/src/runtime/serviceability.rs:4023` prints `VM uptime: <s> seconds`. By reading `VMUptimeDCmd::execute`, HotSpot prints `<s> s` (with the date first under `-date`). **Close:** RECORD row `ob2_cap_vmuptime`, then compare after masking the number.

## 7. `jcmd VM.symboltable` is a constant

The trait default `vm/src/runtime/serviceability.rs:530` answers 0 entries, and `SharedVm` has no override. Unlike the String table, there is no single symbol table to read: the names live in the class files' constant pools.

**Close:** a decision first:
- count the distinct UTF-8 names of the loaded classes' constant pools;
- or answer that the table is not modelled.

RECORD row `ob2_cap_symtab` first.

## 8. `-XX:+PrintFlagsFinal -showversion` prints in the wrong order (opt-in path)

Under `CRATONVM_GEN_PRINT_FLAGS_FINAL=1`, `-showversion` still prints its banner before argument parsing (`run`'s version-query block). The flag table follows when the VM is created. HotSpot creates the VM first (the table), then prints the banner.

`-version` boots the VM and is in order. **Close:** add `-showversion` to `gen_print_flags_final_boots_version`, with the banner printed after `Vm::try_new` and the program then run (the version boot exits today).

## 9. Opt-in builds are Generational-only or JFR-wide by design

Three builds stop short of every collector:
- `VM.stringtable` reads the live table on Generational only;
- `-XX:+PrintFlagsFinal` prints on Generational only;
- the JFR banner prints for every collector, but only behind its opt-in.

The G1 / ZGC halves are their owners' (this round's scope).
