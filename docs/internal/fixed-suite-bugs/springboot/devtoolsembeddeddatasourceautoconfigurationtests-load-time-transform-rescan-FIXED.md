# `DevToolsEmbeddedDataSourceAutoConfigurationTests` — 300s silent HANG: FIXED

**Status: FIXED — 2026-08-09.** 4/4 tests pass. The hang was a throughput
collapse in the `java.lang.instrument` **load-time transform hook**, which
re-offered a class to the registered `ClassFileTransformer` chain on every
*constant-pool resolution* instead of once per class. Not a deadlock, not a
livelock, and — contrary to this page's original triage — **not
Windows-specific**.

The page this replaces is
`known-issues/springboot/devtoolsembeddeddatasourceautoconfigurationtests-silent-hang-20260807.md`.
Its two "ruled out" verdicts were correct and are unchanged; three of its
"not ruled out" candidates are now positively refuted, and its central
inference about what the process was doing was wrong in a way worth
recording — see [What the original triage got wrong](#what-the-original-triage-got-wrong).

## Root cause

`7a7f9826a` (2026-08-06 09:50, *"fix(instrument): a -javaagent transformer is
now offered every class being defined"*) closed a real gap: `addTransformer`
was accepted and then did nothing, because the chain was only ever walked by
`redefineClasses`/`retransformClasses`. A registered transformer never saw a
class *being defined*, so JaCoCo/APM/tracing agents installed and instrumented
nothing.

It hooked the chain at `runtime::instrument::pre_transform_for_load`, called
from `resolve_class_loader_aware` (`vm/src/runtime/interpreter/constants.rs`)
and from `Class.forName`/JNI `FindClass`. That is the **constant-pool
resolution** path — it runs for every `new`, `checkcast`, `instanceof`, field
owner and method owner the interpreter executes, not once per definition. To
stay cheap it approximated "this class is not defined yet, so a definition is
about to follow" with `ClassManager::resolve_fast_path_class_id`, returning
early when the class was already there.

That approximation has a hole with a precise, name-shaped edge
(`classloading/src/class_manager.rs:4486-4514`): when a class is defined by a
**user loader** *and* the same name is also reachable on the built-in
delegation chain, `resolve_fast_path_class_id` deliberately answers `None` —
it will not hand a `UserDefined` ClassId to a request the delegation chain can
answer itself, because that would let a user-loader copy shadow the
application copy globally. So for every class of that shape the early-out
**never fired**, and each resolution paid, in full:

* `find_class_bytes_for_transform` — a jar read + inflate + `to_vec` of the
  whole class file;
* `class_file_supertypes` + a recursive `pre_transform_for_load` per supertype
  and interface — the same read again, per supertype, to `SUPERTYPE_STAGE_DEPTH`;
* `resolve_fast_path_class_id`'s own `find_class_bytes_delegated` probe — one
  more read;
* a Java `byte[]` allocation of the entire class file, and an interpreted
  `invoke_virtual` into every registered `transform`.

### Why *this* class, and why it looked like a spin

Two independent facts have to line up, and this test class is where they do:

1. **Mockito's inline mock maker self-attaches a `ClassFileTransformer`.**
   The `Mockito is currently self-attaching to enable the inline-mock-maker`
   line in the original `.err.log` — read at the time as harmless boilerplate
   marking where the log stopped — is in fact the moment the bug arms. Before
   that line the chain is empty and `transformers_armed` is a single atomic
   load; after it, every resolution enters the hook.
2. **The class carries class-level `@ClassPathExclusions("HikariCP-*.jar")`.**
   That routes its every test method through `ModifiedClassPathExtension` into
   a fresh `ModifiedClassPathClassLoader` (a `URLClassLoader` over a filtered
   copy of the same classpath). Under that loader **every application class**
   — Spring, Mockito, ByteBuddy, JUnit, the test itself — is a user-loader
   class whose name the delegation chain also answers. That is the shadowed
   shape above, for essentially the whole program.

So the process was not spinning. It was making genuine forward progress at a
tiny fraction of its normal rate, re-reading and re-offering class files
thousands of times over. `--stack-sample-ms 1000` on a reproduced hang put
`org/mockito/internal/creation/bytebuddy/InlineBytecodeGenerator.transform` at
`pc=0` as the innermost frame in **~85% of samples**, at a stack depth that
stayed bounded (40-70 frames — no runaway recursion), under a normal Spring
`AbstractApplicationContext.refresh` → `preInstantiateSingletons` →
`MultipleDataSourcesConfiguration.dataSourceOne` → `Mockito.<clinit>` stack.

## Evidence

### The regression is in `dev`, not in Windows

The original page's strongest candidate was "Windows-specific" — every prior
comparison point for this class was Azure Linux. It is not. Both arms below
ran standalone on the same Windows box, one class, no host load, no shard
parallelism:

| Binary | Built | Result |
|---|---|---|
| `cratonvm-fullsuite1shard-20260805.exe` | Aug 5 22:05 | **PASS 4/4, 21.4s** |
| `cratonvm-fullsuite-20260806.exe` (the run that filed the bug) | Aug 6 21:39 | **TIMEOUT, 0-byte stdout** |
| current `dev` @ `681b5c1f1` | Aug 9 | **TIMEOUT, 0-byte stdout** |

Two binaries, one platform, one class — so the variable is the tree, and the
window is Aug 5 22:05 → Aug 6 21:39. `7a7f9826a` lands at Aug 6 09:50, inside it.

### One binary, two arms

The fix ships with `CRATONVM_DBG=load-transform-no-memo`, which restores the
pre-fix "re-offer on every resolution" behaviour. It is the red control, so
the verdict does not rest on comparing two builds:

| Arm | Result |
|---|---|
| default | **PASS 4/4** |
| `CRATONVM_DBG=load-transform-no-memo` | **TIMEOUT at 300s**, 0-byte stdout, `.err.log` ending at the Mockito self-attach line — the filed signature, byte for byte |

### Where the fixed binary lands

This box is shared and its load moves a lot (the same Aug-05 binary measured
21.4s on a quiet box and 43.0s on a busy one), so the numbers below were taken
**interleaved, HotSpot first and last**, so the control brackets the arms:

| Arm | Seconds |
|---|---:|
| HotSpot (run 1) | 6.08 |
| CratonVM, fix applied | **49.6** |
| CratonVM, `cratonvm-fullsuite1shard-20260805.exe` (pre-hook) | 43.0 |
| HotSpot (run 2) | 6.07 |

The fix lands within ~15% of the binary that predates the hook entirely, which
is the right shape: one offer per class is not free, but it is a bounded
one-shot cost rather than a per-resolution one. The remaining 8.2x against
HotSpot is this suite's ordinary Spring-context-startup gap and is not this
bug.

## Fix

`vm/src/runtime/instrument.rs`: claim one load-time offer per class name per
VM, **before** any byte reading, so a repeat resolution is one hash probe and
nothing else.

```rust
if !claim_load_time_offer(shared.vm_identity, name) {
    return;
}
```

Name is the right key, and this is the part worth keeping: the seam this hook
writes through — `ClassManager::stage_transformed_class` /
`pending_transformed_classes` — is **itself name-keyed**, documented as "a
second stage for the same name overwrites the first". A second offer for a
name therefore could never have reached a second definition even in
principle; it could only overwrite bytes staged for the first. Offering once
per name is exactly the granularity the staging mechanism supports, so the
memo costs no coverage the seam could have delivered.

The memo is per VM for the same reason the transformer chain is (one process
can own several heaps), and is dropped by both `forget_vm_transformers` and
`reset_transformer_chain` — an identity a later VM reuses must not inherit the
previous VM's already-offered set, or that VM's agent never sees a class load.

Unit tests pin all four properties (`load_time_offer_is_claimed_exactly_once_per_name`,
`load_time_offer_memo_is_per_vm`, `forgetting_a_vm_drops_its_offer_memo`,
`resetting_the_chain_drops_the_offer_memo`).

## What the original triage got wrong

Recorded because each error was a reasonable reading of the logs, and each
pointed away from the actual mechanism:

* **"No GC activity at all is logged … consistent with a tight, low-allocation
  spin rather than a blocked-on-I/O wait."** The opposite. The hung process
  allocates ~2.4 MB/s (working set 841 MB → 1084 MB over 100s, ~550 page
  faults/s) and does **zero** file I/O in steady state (read/write/other
  operation counts flat at 0 over a 5s window — the jars are already in the
  page cache). Absence of `[moving-young]` lines is not absence of allocation;
  it is absence of a *fallback*, which only logs on the degraded path.
* **"Windows-specific"** — refuted above. The Aug-05 binary passes on this
  same Windows box.
* **"A quiet sibling of the fixed OOB-adapter livelock"** (a speculative
  dispatch reading a field before confirming receiver shape) — refuted. The
  mechanism is class-file re-reading in the instrument hook; no dispatch
  fast path is involved, and the stack samples show ordinary interpretation.
* **"A different bug in the `ModifiedClassPathExtension` nested-`Launcher`
  pathway"** — half right, and the useful half. That pathway is not itself
  buggy here; what it does is *force a user loader*, which is one of the two
  preconditions. The page's instinct to look at the annotation was correct;
  its framing (a bug *inside* the extension) was not.

The single fact that decided it was not in the logs at all: the class had a
21s PASS on an Azure binary four days earlier and a 300s HANG on a Windows
binary the next day, and **nobody had run the older binary on Windows**. One
21-second run collapsed a platform hypothesis into a bisect window.

## Not fixed by this, filed separately

`Log4J2LoggingSystemTests` still times out with this fix applied, and its
`.err.log` contains **no** Mockito self-attach line — no transformer is ever
registered, so this code path is not even armed for it. Whatever that class's
problem is, it is not this one; see
[`log4j2-logback-loggingsystemtests-modifiedclasspath-throughput-hang-20260807.md`](../../../known-issues/springboot/log4j2-logback-loggingsystemtests-modifiedclasspath-throughput-hang-20260807.md),
whose "class-level `@ClassPathExclusions` makes every method re-run a nested
`Launcher`" hypothesis is untouched by this fix and remains open.

## Affected classes

- `module/spring-boot-devtools` — `org.springframework.boot.devtools.autoconfigure.DevToolsEmbeddedDataSourceAutoConfigurationTests`

The blast radius was wider than one class: **every** test class carrying
class-level `@ClassPathExclusions`/`@ClassPathOverrides` that also touches
Mockito was exposed (~40 classes in this tree), as was any run under a real
`-javaagent:` whose application classes are loaded through a custom loader.
