# RESOLVED — the extended interpreter corpus was measuring a VM with no class library

| | |
|---|---|
| **Status** | ✅ RESOLVED 2026-08-05. Root-caused, fixed, and re-baselined. Retired from `docs/known-issues/`. |
| **Area** | `vm/tests/interpreter_tests.rs` (the `CRATONVM_RUN_EXTENDED_INTERPRETER_TESTS=1` corpus) |
| **Original symptom** | Opting in yielded `710 passed; 214 failed` serially, and `STATUS_ACCESS_VIOLATION` / SIGSEGV in the default parallel run. |
| **Now** | `924 passed; 0 failed` serially, and no crash in a full-parallel run, against two pinned lists: 5 real synthetic-library gaps and 11 order-dependent failures. |

## What the original triage got wrong

The doc concluded that the corpus "measures the synthetic JDK, and 214 of its
tests fail there", i.e. that the failures were ~83% synthetic-class-library
gaps. That was half right in a way that mattered.

`test_vm()` builds `VmConfig::new()`, whose JDK mode is
`EMBEDDED_DEFAULT_JDK_MODE` = `JdkMode::Synthetic`. But the ~5,200 synthetic
stubs live behind the `synthetic-jdk` **Cargo feature**, which is not in the
default feature set — and the reproduction command did not pass it. `config.rs`
already documents exactly what that combination is:

> The default Cargo feature set does **not** enable `synthetic-jdk`, so in a
> default build asking for `JdkMode::Synthetic` at runtime yields neither the
> stubs *nor* a boot classpath — a silently broken VM. Callers must check this
> constant (or call `require_synthetic_jdk`) before honouring a synthetic
> request.

`vm-cli` checks it. The in-tree test harness — the other caller — never did. So
the corpus was measuring neither class library.

Measured on Linux, same commit, same fixtures:

| build | result |
|---|---:|
| `cargo test --release -p cratonvm-vm --test interpreter_tests` | 710 passed, **214 failed** |
| …the same, plus `--features synthetic-jdk` | 884 passed, **40 failed** |

174 of the 214 were the missing feature, not a class-library gap.

## The "43 wrong under the real JDK too" cluster was a probe artifact

The original triage re-ran each failing `(class, method)` pair through the
real-JDK **CratonVM CLI** and called the 43 that still misbehaved "genuinely
wrong". That compares CratonVM against CratonVM. Re-run against a real **JDK
25** (`java`, not `cratonvm`) the picture changes completely — of the 214:

| | count |
|---|---:|
| HotSpot produces the value the test expects (so the expectation is right) | **191** |
| `TckJdbc` — HotSpot has no SQLite driver; only CratonVM's own JDBC natives can serve these | 10 |
| `VirtualThreadTest` — the fixture declares its OWN `native` helpers, which only a CratonVM build registers | 10 |
| the expectation itself is wrong — HotSpot disagrees | **3** |

`ScopedValueComplete` "20 wrong in both modes" and `TckJdbc` "every method
returns 0" were both artifacts of asking the wrong oracle.

The three genuinely-wrong expectations were fixed in the fixtures:

* `ReflectionComplete.testFieldGetPrivate` expected `Field.get` on the
  declaring class's own private field to throw `IllegalAccessException`. It
  does not — access is checked against the *calling* class, which here is the
  declaring class. HotSpot returns -1; the very next method in the same file
  already documented the correct rule.
* `PropertiesComplete.testPropertiesLoadSpaces` expected `"  key1  =  value1  "`
  to yield `"value1"`. `Properties.load` strips whitespace before the key and
  around the separator but **not** after the value; HotSpot returns 0.
* `ScopedValueComplete.testThreadVisibility` asserted that a plain child thread
  sees its parent's `ScopedValue` binding, with the comment "in our VM model,
  ScopedValue binding is per-object-field" — a CratonVM implementation detail
  pinned as though it were JEP 506. HotSpot returns 0. This one exposed a real
  VM bug (below).

## The parallel crash: process-global native caches shared by concurrent VMs

Reproduced on Linux as a SIGSEGV in `gen_heap::class_id_of`, reached from
`classloader::cl_load_class_base_delegation_rooted`. The receiver was a
`ClassLoader` object allocated in a **different VM's heap**.

`Vm::new` called `reset_loader_singletons()` / `reset_system_singletons()` /
`reset_classvalue_cache()` to clear "stale ObjectRefs from a previous VM
instance". That is correct only while VMs are created and disposed of strictly
in sequence. A Rust test binary runs its `#[test]`s on several threads, so two
`Vm`s are routinely *concurrently* live: VM B's constructor wiped the cell VM A
was using, VM A rebuilt it, and whichever read next dereferenced an address in
the other's heap.

Five process-global tables were converted to per-`vm_identity` rows
(`cratonvm_native_api::vm_scoped::VmScoped`, added for this, following the
`security_manager::VmSecurityState` precedent) and torn down from
`release_vm_native_state`:

| table | why it aliased |
|---|---|
| `classloader`'s app/platform `ClassLoader` singletons | one cell per process; `Vm::new` reset it |
| `lang_class`'s annotation-proxy cache, its child roots, and the last-proxy interfaces array | keyed by `ClassId`, which every VM mints from zero |
| `lang_system`'s `System.getenv()` / `getProperties()` singletons | one cell per process; `Vm::new` reset it |
| `phases_late`'s `ClassValue` memoization cache | keyed by a pair of 32-bit identity hashes |
| `lib.rs`'s `ReentrantLock` state table (and the new `ReentrantReadWriteLock` one) | keyed by a 32-bit identity hash |

Their GC scan/remap hooks now take a `vm_identity` too — handing one VM's
object to another VM's collector as a root was the same bug wearing a hat.

After the change: five consecutive full-parallel runs, no crash. The corpus
also runs ~8× faster than it did serially.

## What else was fixed on the way

* **`FileInputStream.<init>(Ljava/io/File;)V` was never registered** in
  synthetic mode (the `FileOutputStream` side had had both `File` overloads
  since FOS-FIX). Seven `TckIo` tests.
* **`fis_set_fd` trusted a `set_field_by_name` that silently no-ops.** The
  synthetic `java/io/FileDescriptor` is `instance_fields(4)` — four `_fN` slots,
  no `fd`, no `handle` — so the descriptor `fis_ensure_fd_object` attached could
  not hold the id, both writes vanished, and every subsequent `read()` answered
  -1. It now reads the value back and falls through to the legacy slot-0
  encoding when the write did not land.
* **The synthetic AQS natives were unreachable in synthetic mode.** Real AQS is
  the right default *for real-JDK mode*, where there is
  `AbstractQueuedSynchronizer` bytecode to defer to; synthetic mode has none, so
  `new Semaphore(3)` and `new ReentrantLock()` raised `UnsatisfiedLinkError`.
  14 `JucComplete` tests. Registration is now runtime-gated on
  `use_synthetic_jdk`, so a feature-enabled binary running real-JDK mode is
  unaffected.
* **`ReentrantReadWriteLock` had a declared layout and four dead, unregistered
  stubs** with no `lock`/`unlock` at all. Implemented over the same
  monitor-parking scheme the `ReentrantLock` natives use.
* **`ScopedValue` bindings were visible to every thread.** They are now recorded
  against the binding thread, so a plain child thread sees the value as unbound,
  as JEP 506 specifies.
* **Fixture hygiene**: `JitSafepointStress` / `ManifestNullValue` declare the
  package their directory implies; the empty `ToolProviderProbe.java`/`.class`
  and the orphan `PgoTest$Rect/$Shape/$Square.class` (committed with no
  corresponding source, `this_class` in the default package) are gone.

## The two gates the corpus now has

Both live in `require_extended_interpreter_tests`:

1. Opting in without `--features synthetic-jdk` is a hard failure with the
   explanation, instead of a meaningless 214.
2. Opting in with no staged fixtures is a hard failure too. `vm/build.rs`
   compiles every fixture of a pass in ONE `javac` invocation, so a single
   unbuildable source stages nothing — which is how the corpus stayed dark
   until `f715d1367`, printing a green `924 passed` that had run none of it.

## The pinned baseline — and why it is two lists

The 16 remaining `(class, method)` pairs were each verified against real JDK 25,
and then verified a second way: **run alone** in a fresh process, via
`--exact <test>`. Eleven of them PASS that way. They are not missing features —
the feature works when nothing else has run first — so they moved to
`KNOWN_ORDER_DEPENDENT` and are tracked as the non-fatal tail of the very
VM-lifecycle defect this document is about
(`docs/known-issues/corpus-is-order-dependent-20260805.md`).

That leaves **5** genuine gaps in `KNOWN_SYNTHETIC_JDK_GAPS`, all in the
`java.lang.reflect.Proxy` / annotation-proxy surface
(`docs/known-issues/synthetic-jdk-class-library-gaps-20260802.md`). It is a
**two-way** gate: an unlisted mismatch fails the run, and a listed pair that
starts passing also fails the run, telling you to delete the entry.
`KNOWN_ORDER_DEPENDENT` deliberately does not trip on an unexpected pass,
because execution order decides it.

The second check is the transferable part. "Fails in the suite" and "is a
missing feature" are different claims, and eleven of sixteen entries here were
the first without being the second.

## Reproducing (current)

```bash
CRATONVM_RUN_EXTENDED_INTERPRETER_TESTS=1 \
  cargo test --release -p cratonvm-vm --features synthetic-jdk --test interpreter_tests
```

`--test-threads=1` is no longer needed. A mismatch now reports the fixture, the
method, the expected value and the thrown exception's class and detail message
(`Vm::describe_result`), instead of `Err(ExceptionThrown(ObjectRef { .. }))` —
an address with no class, message or stack, which is why 175 of the original
214 failures had to be re-run one at a time through a hand-written Java probe
before they could even be grouped.
