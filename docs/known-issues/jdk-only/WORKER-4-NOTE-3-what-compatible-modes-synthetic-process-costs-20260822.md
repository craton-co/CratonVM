# WORKER-4-NOTE-3 — what compatible mode's synthetic `Process` actually costs: 34 cases, and the whole `ProcessHandle.Info` surface

**Status: MEASURED. No source change — a REFUSAL, and a correction of this
lane's own reasoning.** 2026-08-22, Linux (Azure host 2), Temurin 25.0.4+7.

## 1. The measurement, which is what is new here

`regression-suite/probes/W4Process.java` (added by `WORKER-4-3`): 34 behavioural
cases over `ProcessBuilder.start()` — exit codes, stdout / stderr / stdin, both
redirect forms, `redirectErrorStream`, environment, working directory,
`waitFor(timeout)`, `destroy`, `destroyForcibly`, `onExit`, `pid` / `toHandle`,
the whole `ProcessHandle.Info` surface, and three failure modes.

```text
  CratonVM --jdk-only     34 / 34 against the oracle
  CratonVM --real-jdk     27 / 34
```

The seven are not seven preferences:

```text
                              HotSpot            --jdk-only         --real-jdk
  getClass()                  ProcessImpl        ProcessImpl        cratonvm.synthetic.Process
  info().command()            present            present            ABSENT
  info().commandLine()        present            present            ABSENT
  info().arguments()          present            present            ABSENT
  info().user()               present            present            ABSENT
  info().startInstant()       present            present            ABSENT
  start() on an empty command AIOOBE             AIOOBE             IllegalStateException
```

**Compatible mode answers an entirely EMPTY `ProcessHandle.Info` for a live
child.** `command`, `commandLine`, `arguments`, `user` and `startInstant` are
all absent where HotSpot and this VM's own strict mode have all five. That is a
capability lost, not a house style kept — and `cratonvm.synthetic.Process` is a
class name that exists in no JDK, which is what a log line and a serialized
record carry.

The `info()` case needs a LIVE child to be a real measurement: `info()` reads
`/proc/<pid>`, which is gone once the child is reaped, so asking about a `true`
that has already exited measures a spawn-cost race between two VMs rather than a
capability. The probe uses `sleep 2` and destroys it afterwards. An earlier draft
did not, and reported a difference that was partly the race.

## 2. Why this is a REFUSAL, and the mistake that got here

The obvious remedy is to let compatible mode take the path strict mode already
takes — especially since the machinery is not strict-mode-only.
`--dump-native-registry` shows it registered `Bridge` in BOTH modes and already
carrying traffic: `ProcessImpl.forkAndExec` 17 invocations,
`ProcessHandleImpl.waitForProcessExit0` 16, `isAlive0` 17, `destroy0` 3,
`Info.info0` 1.

This lane tried it — made `native_process_builder_start` yield to bytecode when
`java.lang.ProcessImpl` is loadable — built it, and got:

```text
IncompatibleClassChangeError: array receiver does not implement the requested
interface java/util/List (dispatching java/util/List.toArray(…))
  at java/lang/ProcessBuilder.start(ProcessBuilder.java:1046)
```

**`native-builtins/src/phases_late.rs` documents that outcome, in the file, next
to the registration:**

> *"The tag is what makes `--jdk-only` coherent, and the cluster has to move
> together. Restating `start()` alone leaves `<init>([Ljava/lang/String;)V` in
> place, which writes the raw `String[]` into the `command` field; the JDK's own
> `start()` then reaches `command.toArray(...)` on an array and dies …
> **Measured, not predicted — that is precisely what the first build with only
> `start` restated did.**"*

So this lane spent a build re-deriving a documented fact. The brief says it
plainly — *"Grep before asserting the tree does not know. Repeatedly, the thing a
lane 'discovered' was already documented as deliberate."* — and the grep that
would have caught it is `ProcessBuilder` in `phases_late.rs`, not just the
`start` body in `process.rs`. **Read the whole cluster's registrar before
changing one member of it.** The change is reverted; nothing of it landed.

## 3. What the next lane needs, and what it now has that it did not

The remedy is to move **all eleven** `ProcessBuilder` registrations together —
the constructors first, because `<init>([Ljava/lang/String;)V` writing a raw
`String[]` into a field the JDK declares `List<String>` is the specific thing
that breaks the JDK's `start()`.

What this note adds to that decision:

* **A price tag.** The existing comment justifies the cluster's TAG and says
  compatible mode is *"deliberately unchanged"*. It does not say what unchanged
  costs. It costs the whole `ProcessHandle.Info` surface and the class name — 7
  of 34 cases — and that had never been measured.
* **A gate.** `W4Process.java` is in the tree. A lane moving the cluster can
  measure the before and after in one command instead of arguing about it, and
  the target is unambiguous: 34/34, which strict mode already reaches.
* **The evidence that the destination works.** Strict mode is not a different
  implementation; it is the same `Bridge` natives with the stub removed from in
  front of them.

This lane does not move the cluster. `java.lang.ProcessBuilder`'s registrar is
`native-builtins/src/phases_late.rs`, the change is eleven registrations plus
whatever the constructors' field writes are load-bearing for, and it is a
compatible-mode behaviour change on a path every application suite uses.
`[a refusal with evidence beats a retirement without it]`.
