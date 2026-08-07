# The `native-io` `Bridge` registrations the image does not back — L5's residual

**Status:** OPEN — reclassification questions, filed 2026-08-05 by the L5
`register_with_kind` migration. Nothing here is a crash; every row named below
behaves today exactly as it did before L5. What is open is that each one is
tagged `Bridge` while the JDK 25 image says its target is not an `ACC_NATIVE`
method, so `--jdk-only` admits it on a claim nobody has checked.

> **MOSTLY CLOSED 2026-08-06 — re-measured, and the last statement row landed.**
> Read this banner before the body: three of the five items under *What would
> close this* are done, and one of the remaining two was filed in the wrong
> column.
>
> * **The statement pass is finished.** L5's own criterion — a row may state
>   `Bridge` exactly when the image declares that triple `ACC_NATIVE` — now
>   selects **zero** rows tree-wide. It was down to one:
>   `java/io/UnixFileSystem.list0`, whose `for name in ["list", "list0"]` loop
>   called `r.register` while fourteen sibling loops in the same function body
>   went through the `fs_reg` helper that states the kind. Routed through the
>   helper: `ACC_NATIVE`-backed-and-stated 773 → 774, total `bridge` stated
>   835 → 837 (the extra one is `WinNTFileSystem.list0`, `ABSENT` on a Linux
>   image and deliberately stated anyway, because `FS_IMAGE_NATIVE` carries both
>   spellings and the platform question is settled elsewhere).
> * **`setDirect0`, the Windows census, and `SocketDispatcher.close` are all
>   answered** — see
>   [`census-asks-one-class-on-one-platform.md`](census-asks-one-class-on-one-platform.md),
>   which also found that **76 % of the `undecl` bucket is not dead** (1,612
>   inherited shadows, 308 abstract, 19 uncredited bridges; only 603 dead).
> * **The `java.lang.Process` abstract-registration hazard is inert, and the
>   hazard is the CONCRETE registrations instead.** An abstract method must be
>   overridden by any concrete subclass, so dispatch finds the override and never
>   reaches the native. The concrete ones are what a subclass does *not*
>   override, so dispatch walks up to `java/lang/Process` and the native wins —
>   returning `isAlive()==false` for a live process and `pid()==0` where the spec
>   requires `UnsupportedOperationException`. Filed with a committed repro:
>   [`process-natives-answer-for-user-subclasses-FIXED-20260806.md`](../../internal/process-natives-answer-for-user-subclasses-FIXED-20260806.md).
>   Item 5 below has the model backwards.
> * **The row counts in this file are ~11 % too large.** The census emits one row
>   per *registration*, not per slot, so a triple registered twice appears twice
>   and only the last can dispatch. 1,237 of 11,876 rows own no slot; 1,092 of
>   them are in the unadjudicated `Bridge` population. `java/lang/Process` is the
>   visible case: 21 rows for 13 methods, because `native-io/src/process.rs`
>   overwrites `native-builtins/src/phases_late.rs` for eight of them — so this
>   file's "13 rows on `java.lang.Process` itself" is the method count, not the
>   row count. `owns_slot` is now a census column and
>   `scripts/jdk-only-adjudicate.py` prints the split.
>
> **Item 1 is CLOSED 2026-08-06** —
> [`synthetic-process-cluster-RETIRED-20260806.md`](../../internal/synthetic-process-cluster-RETIRED-20260806.md).
> The cluster was **37 rows and not 25** (the count below omits
> `cratonvm/synthetic/AnonymousObject$2`, 4 rows, and miscounts the pipe
> streams), and the `Bridge` tag was wrong by §1.5's own definition since no
> image on either platform declares these classes at all. All 37 are
> `SyntheticStub` now, and `--jdk-only` returns a real `java.lang.ProcessImpl`
> from `ProcessBuilder.start()`, byte-identical to HotSpot 25 across a 21-line
> surface probe. Classes fabricated in strict mode on a subprocess workload: 4
> before, **0** after.
>
> Retagging alone would not have done it, and the retired record is worth
> reading for what else it took: `ProcessImpl.forkAndExec` had to be written
> (the registration that existed was on the pre-JDK-9 `UNIXProcess`, unreachable
> and off by one), the VM's process handle is not a pid, `destroy()` had been
> silently losing to the JDK's reaper thread, and three `java.io` constructor
> shims skip the real `closeLock` initializer so every stream built through them
> throws NPE — not IOException — on its first close. Four of those five were
> found by running it.
>
> The mode-independent supertype defect that record also carried — the class not
> being in its own `getSuperclass()` chain while `isAssignableFrom` said it was —
> was fixed earlier the same day and still governs what compatible mode returns.
>
> **Still open from this file:** nothing else. Item 5's model was wrong (see
> above), items 2-4 are answered, and item 1 now has a verdict and an ordered
> plan rather than a question.

> **PARTLY SUPERSEDED 2026-08-05 by a wider measurement.** Three verdicts
> below rest on a census that asks one class in one image, and two of them do
> not survive:
>
> * The `sun/nio/ch/FileDispatcherImpl` alias rows are **not** "robustness
>   aliases, not adjudicated bridges" — they inherit the ACC_NATIVE syscall
>   surface from `UnixFileDispatcherImpl`, which is exactly what §1.5 means.
>   They state their kind now.
> * `sun/nio/ch/WindowsFileDispatcherImpl` is **not** "the same syscall layer
>   on a Windows image". It is on **no** JDK 25 image; the Windows JDK names
>   that class `FileDispatcherImpl` too. All 28 rows are dead everywhere.
> * `SocketDispatcher.close` — "dispatched 3× while resolving to no declared
>   method", flagged here as the row worth a second look — resolves to concrete
>   bytecode on `sun.nio.ch.UnixDispatcher`. It is an ordinary §1.4 shadow.
> * `setDirect0` was **not** a wrong descriptor: the registered
>   `(FileDescriptor, CharBuffer)I` form is the *Windows* signature. What was
>   missing is the Unix `(FileDescriptor)I` entry point, now registered on its
>   declarer, so `ExtendedOpenOption.DIRECT` is told "unsupported" instead of
>   dying on `UnsatisfiedLinkError`.
>
> The `RandomAccessFile.close0` and `WindowsSocketOptions` verdicts stand — the
> first is dead in the whole hierarchy, the second is a genuine Windows bridge.
> See [`census-asks-one-class-on-one-platform.md`](census-asks-one-class-on-one-platform.md).

Sibling record for the crates L5b/L5c did next:
[`l5bc-awt-builtins-bridge-residuals.md`](l5bc-awt-builtins-bridge-residuals.md).

## What L5 did, and why this file exists

Lane L5 of the wave-2 plan
([`docs/feature-designs/jdk-only-wave2/`](../../feature-designs/jdk-only-wave2/README.md);
the lane doc itself is retired, as the `jdk-only-wave2-L5-nativekind-native-io`
write-up) migrated the four `JDK-ONLY-CLASSIFY: bridge` registrars in `native-io` from the
ambient `set_category(Bridge)` to `register_with_kind(.., NativeKind::Bridge)`,
so the kind is a fact stated at the registration site rather than one inherited
from an enclosing frame. Its step 3 says:

> Confirm the static adjudication agrees: for rows you tag `Bridge`,
> `image_declaring_method.acc_native` should be `true`. […] Where it is `false`,
> the marker is wrong and this is a reclassification question, not a migration
> one.

That check was run per row, from a schema-3 census, and it split the four
registrars in half. **87 of the 204 registrations are ACC_NATIVE on the image
and now state their kind. The other 117 do not, and this file is the list.** They
were deliberately left on the ambient category: leaving them inherited is what
keeps the census able to say "nobody adjudicated this", which is exactly the
property the record
the retired `native-kind-is-ambient-and-defaults-to-syntheticstub` write-up
exists to protect. Stating `Bridge` on all 204 would have been a codemod that
made the census report 204 adjudicated bridges where the truth is 87.

**None of these rows changed kind.** L5 does not reclassify anything; contract
§8 puts that in a different wave. `BASELINE_SYNTHETIC_STUBS` is unmoved at 157.

## How to reproduce the table

```sh
cratonvm --real-jdk --java-home <JDK25> --explain-jdk-only \
    --dump-native-registry census.json -cp probes JdkOnlyCensusLoadProbe
python3 scripts/jdk-only-adjudicate.py census.json
```

`--explain-jdk-only` is not optional; without it `image_declaring_method` is
`null` for every row and the script refuses. The per-registrar breakdown below
was taken from that census on JDK 25 (Linux, `x86_64`) on 2026-08-05, and every
claim about the image was independently confirmed with `javap -p`.

**Read the counts below as per-*registrar*, not per-file.** Grouping the same
census by source file gives larger numbers for two of them — `net.rs` 34 and
`nio_native.rs` 69 — because each file also holds registrars that are not
`bridge`-marked and are therefore outside L5's scope (`net.rs`'s
`java.net.MulticastSocket` block, 13 rows; `nio_native.rs`'s async-channel
block, 16 rows, which carries its own `JDK-ONLY-CLASSIFY: unknown` marker).
`process.rs` (42) and `random_access_file.rs` (1) are the same either way.

The verdict column is the census's, in the adjudicate script's vocabulary:

| verdict | meaning |
|---|---|
| `ABSENT` | the image has no such class at all |
| `UNDECL` | the class is present and does not declare this method+descriptor |
| `CODE` | the target has concrete bytecode — the native shadows it (§1.4) |
| `ABSTRACT` | the target is abstract — the native intercepts every implementor |

## `net.rs` — `register_sun_nio_ch_net`, 21 of 67

The best-evidenced registrar in the crate: 46 rows are genuine socket syscalls
and now say so. The residue is three distinct things.

**Platform variant (9 rows, `ABSENT`).** Every `jdk/net/WindowsSocketOptions`
registration — `keepAliveOptionsSupported0`, `getIpDontFragment0`,
`setIpDontFragment0`, and the six `*TcpKeepAlive*` accessors. The class does not
exist in a Unix image; the *same* registrar's `jdk/net/LinuxSocketOptions` twin
is `ACC_NATIVE` for all 15 of its entries and is stated. These are not dead
code — they are the correct registrations on a Windows image, where the verdicts
would be exactly reversed. **The open question is not "delete them", it is that
a census taken on one platform cannot adjudicate the other platform's rows, and
nothing in the tooling says so.** Any wave that flips the default kind must not
read `ABSENT` here as "stub".

**Method not declared (11 rows, `UNDECL`).**

| registration | note |
|---|---|
| `sun/nio/ch/Net.socket0(ZZZ)I` | the pre-`fastLoopback` arity; JDK 25 has only `(ZZZZ)I` |
| `sun/nio/ch/Net.close(Ljava/io/FileDescriptor;)V` | `Net` has no `close`; the real teardown is `UnixDispatcher.close0`, which *is* registered and stated |
| `sun/nio/ch/Net.read0` / `write0` | `Net` declares neither; `SocketDispatcher` does, and those two rows are stated |
| `sun/nio/ch/SocketChannelImpl.read0` / `write0` | belt-and-braces aliases; the class does not declare them |
| `sun/nio/ch/ServerSocketChannelImpl.read0` / `write0` | same |
| `sun/nio/ch/SocketDispatcher.close0(I)V` | an int-fd descriptor that matches nothing on this image |
| `sun/nio/ch/SocketDispatcher.invalidateAndClose` | not declared |
| `sun/nio/ch/SocketDispatcher.close(Ljava/io/FileDescriptor;)V` | **not declared, and dispatched 3× in the census run** |

That last one is the row worth a second look, and it is the one the previous
marker's "dead registrations" framing would have got wrong. A registration that
the image does not declare *and that the VM nonetheless dispatches* is not dead:
something is resolving to it through a path that does not consult the image.
Whether that is receiver-driven dispatch finding the name on a class-chain
walk, or the registry answering a lookup the JDK bytecode would have answered
itself, is not established here. Do not delete it on the strength of `UNDECL`.

**Shadow (1 row, `CODE`).** `sun/nio/ch/NativeDispatcher.preClose(Ljava/io/FileDescriptor;JJ)V`
has concrete bytecode in the image. Contract §1.4 lets a `Bridge` lose to real
bytecode, so it is not wrong today, but it is a §1.5 `Bridge` in name only.

## `nio_native.rs` — `register_nio_natives_real`, 53 of 75

This registrar deliberately registers one set of file-descriptor natives under
three platform class names, and the census makes the consequence precise: **on
any given image at most one of the three names is the `ACC_NATIVE` declarer.**

| class | rows | what the image says |
|---|---:|---|
| `sun/nio/ch/UnixFileDispatcherImpl` | 21 | 14 `ACC_NATIVE` (stated), 7 `UNDECL` |
| `sun/nio/ch/FileDispatcherImpl` | 21 | 1 `ACC_NATIVE` (`init0`, stated), 20 `UNDECL` — it `extends UnixFileDispatcherImpl` and inherits the rest |
| `sun/nio/ch/WindowsFileDispatcherImpl` | 21 | 21 `ABSENT` |

The registrar's block is now split so the 15 stated rows are the ones their own
class declares. The 6 methods that resolve to no `ACC_NATIVE` target on *any* of
the three are a separate finding, and two of them are real descriptor bugs:

* `write0(Ljava/io/FileDescriptor;JIZ)I` and
  `writev0(Ljava/io/FileDescriptor;JIZ)J` — the trailing `boolean` is an older
  JDK's signature. JDK 25 declares `write0(..JI)I` / `writev0(..JI)J`, both of
  which are also registered here and are stated.
* `close0` and `preClose0` — declared on `sun.nio.ch.UnixDispatcher`, not on the
  file dispatchers. `net.rs` registers them on the right class and states them
  there, so the surface is covered; these three-class copies are not.
* `duplicateHandle(J)J` — Windows-only.
* **`setDirect0(Ljava/io/FileDescriptor;Ljava/nio/CharBuffer;)I` — a genuine
  descriptor mismatch.** JDK 25 declares
  `UnixFileDispatcherImpl.setDirect0(Ljava/io/FileDescriptor;)I`. The
  registration here can never bind. It has never been dispatched in any census
  run, so nothing is currently broken by it, but a real caller would get an
  `UnsatisfiedLinkError` for a native this crate believes it provides. Fixing
  the descriptor is a behaviour change and therefore not L5's to make.

That accounts for 48 of the 53 (7 `UNDECL` on the Unix name, 20 on the leaf,
21 `ABSENT` on the Windows name). The last 5 are
`sun/nio/ch/FileChannelImpl.position0` / `allocationGranularity0` / `initIDs`
(`UNDECL` — legacy names carried for older JDKs) and
`sun/nio/ch/NativeThread.current()J` / `signal(J)V` (`CODE`; JDK 25 declares
`current0()J` natively — that one is stated — and `current()`/`signal()` as
bytecode wrappers).

The same file's async-channel block (16 further rows) is *not* counted here: it
carries its own `JDK-ONLY-CLASSIFY: unknown — needs census` marker and is out of
L5's scope, which is the `bridge`-marked registrars only.

## `process.rs` — `register_process_natives`, 42 of 51

Nine rows are the real thing and now state it: `ProcessHandleImpl.initNative`,
`getCurrentPid0`, `isAlive0`, `waitForProcessExit0`, `destroy0(JJZ)Z`,
`parent0`, `getProcessPids0`, and `ProcessHandleImpl$Info.initIDs` / `info0`. A
subprocess cannot be spawned or reaped from bytecode.

The other 42 are the most interesting residue in the crate, because **25 of them
are `Bridge` registrations on a class the image does not contain and the VM
mints**: `cratonvm/synthetic/Process`, `…/ProcessExitWaiter`,
`…/ProcessPipeInputStream`, `…/ProcessPipeOutputStream`. `Bridge` is what keeps
them alive under `--jdk-only`, and under `--jdk-only` contract §5 forbids
fabricating exactly that kind of class. This is the same unresolved shape as the
`Function$Identity` successor defect in
the retired `native-kind-is-ambient-and-defaults-to-syntheticstub` write-up:
a surviving `Bridge` whose receiver class the policy says may not exist, held
together today only because
[`ensure_synthetic_class` cannot enforce](ensure-synthetic-class-cannot-enforce-only-record.md).
Neither half can be fixed alone. **This is the single largest reclassification
question L5 turned up, and it is not a `native-io` question — it is the same
`Bridge`-vs-synthetic-receiver hole, found in a second place.**

The rest:

* 13 rows on `java.lang.Process` itself — 6 abstract (`waitFor()I`,
  `exitValue`, `destroy`, `getInputStream`, `getErrorStream`,
  `getOutputStream`) and 7 with concrete bytecode (`waitFor(JLTimeUnit;)Z`,
  `isAlive`, `destroyForcibly`, `pid`, `toHandle`, `descendants`, `onExit`).
  The abstract ones intercept **every** implementor, including a user subclass
  of `Process` — the same hazard `register_interface_natives` carries.
* `java/lang/ProcessImpl.create(…)J` (`UNDECL`) — the Windows spawn entry
  point. The Linux image declares `forkAndExec` instead.
* `java/lang/UNIXProcess.forkAndExec` (`ABSENT`) — the class was removed in
  JDK 9; this registration cannot bind on any supported image.
* `java/lang/ProcessHandleImpl.destroyProcess0(JZ)Z` (`UNDECL`) — superseded by
  `destroy0(JJZ)Z`, which is registered next to it and is stated.
* `java/lang/ProcessBuilder.start()` (`CODE`) — a deliberate shadow of real
  bytecode, routing to this crate's spawn path.

## `random_access_file.rs` — `register_random_access_file_natives`, 1 of 11

Ten of eleven are `ACC_NATIVE` on `java.io.RandomAccessFile` and are stated.
The eleventh, **`close0()V`, is not declared by JDK 25 at all** — the class
closes through `FileCleanable`/`fd`, and `close()` is ordinary bytecode. It is
left on the ambient category, which is why the `set_category` scope survives in
that registrar for a single registration.

Note that the marker on this function previously read "8 of the 11 … (`open0`,
`read0`, `readBytes`, `write0`, `writeBytes`, `getFilePointer`, `seek0`,
`length`, `setLength`, `initIDs`)" — ten names, a count of eight, and three
spellings (`readBytes`, `length`, `setLength`) that this crate does not
register and JDK 19+ does not declare. That is what a marker written from a
static `javap` read of the wrong JDK looks like, and it is the reason step 3 of
L5 exists.

## What would close this

Nothing here is closed by more `register_with_kind` calls. In rough order of
value:

1. **The `cratonvm/synthetic/Process*` cluster.** Adjudicate `Bridge`-tagged
   registrations whose receiver class is VM-minted, as a class of defect, in
   both places it is now known to occur.
2. **`SocketDispatcher.close(Ljava/io/FileDescriptor;)V`.** Establish what
   dispatches it, given the image does not declare it.
3. **`setDirect0`'s descriptor.** A one-line fix, but a behaviour change.
4. **A Windows-image census.** Until one exists, `ABSENT` on a
   `Windows*`-named class means "not measured", not "dead", and no automated
   pass may treat the two the same.
5. **The abstract-method registrations on `java.lang.Process`**, which belong
   with `register_interface_natives`' still-open receiver-class question.

## Verification that L5 itself was inert

Measured 2026-08-05 on Linux/JDK 25, two release binaries built from the same
tree with and without the change.

* **Census diff, `--real-jdk`:** per-kind registration totals identical
  (`intrinsic 687, bridge 10842, synthetic-stub 387, total 11916`); **zero
  `kind` changes** across all 10,780 distinct triple+kind rows; `kind_stated`
  9 → 96, i.e. **+87 exactly**, every newly-stated row `bridge`, and all 87
  attributed to the four `native-io` registrars (net 46, nio_native 22,
  random_access_file 10, process 9).
* **Census diff, `--jdk-only`:** same, on 11,529 rows with
  `synthetic-stub: 0` before and after — the strict gate still refuses exactly
  what it refused.
* **`CRATONVM_NO_STUBS=1` + dropped-stub listing, both arms:** boot succeeds,
  the probe completes all 9 sections, and the drop lists are **byte-identical
  at 436 entries**. This is the check the 2026-07-14 `java.util.Properties`
  regression would have failed.
* **`native-builtins/tests/stub_ratchet.rs`:** `BASELINE_SYNTHETIC_STUBS = 157`,
  `SLACK = 0`, unmoved — not re-baselined, not touched.
* **L6's `regression-suite/bridge-ratchet.sh`:** PASS, unmoved at 10,069 /
  4,755. **This is the expected result, not a shortfall.** That ratchet counts
  `Bridge` rows with *no* `ACC_NATIVE` target; the 87 rows L5 stated are exactly
  the rows that DO have one, so they were never in the 10,069. Only a
  reclassification can move L6's number — a migration that moved it would have
  done so by stating `Bridge` on rows the image does not back.

Reproduce the diff with `scripts/jdk-only-adjudicate.py` on two censuses, or
compare `kind_stated` per `registered_by` file directly.
