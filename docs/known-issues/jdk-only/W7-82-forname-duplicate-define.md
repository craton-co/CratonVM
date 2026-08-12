# W7-82 — a second `Class.forName` through a bare `URLClassLoader` re-drove the define

**Status: FIXED 2026-08-12, with a vector and a probe.** A repeated
`Class.forName(name, true, loader)` — or `loadClass` — through **one bare
`java.net.URLClassLoader`** failed on CratonVM with
`IncompatibleClassChangeError: class X already defined by user-defined(N)
loader`, surfaced as `ClassFormatError` out of `URLClassLoader.findClass`.
HotSpot returns the cached class. Reproduced in **both** modes, fixed in one
shared helper.

Branch `fix/forname-duplicate-define-20260812`, off dev `31caacc1b`.

## The record that reported it was right about the observation and silent on the cause

W7-79-loadlibrary-compatible-arm.md filed this in one paragraph as a probe trap
found in passing, out of that lane's scope. It named the symptom exactly and
made no claim about the mechanism. Both halves were checked before anything was
written, because this directory has produced nine records whose observation was
right while the prescribed fix was wrong.

## Reproduced, measured, and narrowed

`probes/ForNameCacheProbe.java` + `probes/ForNameCacheProbeTarget.java`
(compiled into a directory OFF the application classpath — group 0 asserts the
target really is invisible to the app loader, or every "distinct class" result
below is manufactured). One binary, `target/release/cratonvm` at dev
`31caacc1b`, HotSpot 25.0.3 beside it, the mode flag as the only variable.

| group | shape | HotSpot | `--jdk-only` | `--real-jdk` |
|---|---|---|---|---|
| 1 | `forName` x3, one bare `URLClassLoader` | same `Class` | **`ClassFormatError` on #2 and #3** | **same** |
| 2 | `loadClass` x2, same loader | same `Class` | **`ClassFormatError`** | **same** |
| 3 | two independent loaders, `forName` x2 each | 2 distinct, both work | distinct OK, **repeat throws** | **same** |
| 4 | application loader, `forName` x2 (and 1-arg) | same `Class` | PASS | PASS |
| 5 | boot loader, `forName` x2 | same `Class` | PASS | PASS |
| 6 | child loader delegating to app, `forName` x2 | parent's `Class` | PASS | PASS |
| 7 | duplicate `defineClass`, one loader | `LinkageError` | PASS | PASS |

`PROBE result=OK` (23 checks) on HotSpot; `FAILED(10)`, identical, on both
CratonVM arms.

**Which loader path.** Only a **bare** `new URLClassLoader(...)`. Not the app
loader, not the boot loader, not a delegating child. And — the discriminator
that named the cause — **not a `URLClassLoader` SUBCLASS**:

```
DISC bare URLClassLoader              -> THREW ClassFormatError ... already defined by user-defined(3) loader
DISC SUBCLASS of URLClassLoader       -> same=true
```

Same URLs, same parent, same call, one line of `extends` between them, on both
arms. HotSpot answers `same=true` for both.

**Which call shape.** `forName(String, boolean, ClassLoader)` and `loadClass`
both fail, because both reach `URLClassLoader.findClass`. `forName(String)` and
the app/boot loaders never do. `defineClass` is a different road and was
already correct.

**Genuinely re-defined, or only reported so?** Neither, exactly — and the
distinction is the fix. The class is **not** re-defined: the define is genuinely
**re-driven**, reaches `define_class_full`, and the duplicate-define check
rejects it correctly. The defect is one rung earlier: the **cache probe ahead of
it went blind**, so a lookup that should never have reached a define did.

**Did a prior duplicate-define fix over-correct into this?** No — checked,
not assumed. `git log -S "already defined by"` dates the class manager's
duplicate check to the initial commit, and the 2026-08-11 fix recorded in
duplicate-defineclass-served-the-mirror-instead-of-linkageerror-FIXED-20260811.md
changed `lang_system`'s `defineClass0/1/2` road only — a road this defect never
touches. The producer is **`978799522`, 2026-07-01, "Fix builtin loader user
namespace lookup"**, which added the `loader_id_of_class(cid) > 2 -> hide`
clause. Its own two tests instantiate `java/lang/ClassLoader`; neither
instantiates a `URLClassLoader`, which is why the case went unseen for six
weeks. Group 7 of the probe is the guard that the 08-11 rule is still enforced,
and it passed before the fix and must keep passing after it.

## The cause, in one sentence

`is_builtin_loader_class` lists `java/net/URLClassLoader`, so
`is_user_defined_loader` says *false* for a bare instance — but the namespace
allocator disagrees, and the two halves then contradicted each other:

* `loader_namespace_id_at` and `peek_loader_namespace_id` both spell their guard
  `!is_user_defined_loader(..) && !is_bare_url_class_loader(..)`, so a bare
  `URLClassLoader` **defines into its own namespace id (>= 3)**;
* `find_loaded_class_for_loader_inner` was the one site left out of that
  carve-out, so it took the built-in branch, whose `loader_id_of_class(cid) > 2
  -> return None` clause **hid namespace-3 classes from the loader that defined
  them**.

`URLClassLoader` is the only entry on that list this can happen to:
`java/lang/ClassLoader` is abstract and `java/security/SecureClassLoader`'s
constructors are protected, so an instance of either is necessarily a user
subclass carrying a user class name. The carve-out is complete, not arbitrary.

`ucl_try_define_local_class` calls the blind function at **four** sites — the
pre-check, the double-checked probe under the define lock, and both post-define
"we merely lost the race, return the winner's class" recovery arms, one of which
carries the comment *"JVMS §5.3.5 says the loser observes the winner's class,
not a linkage error."* The intended behaviour was already written down four
times over. One blind instrument defeated all four.

## Which registration wins

Grepped, then read, then **measured** — not brace-scanned. (Indentation is
actively misleading in this tree: a column-0 `}` inside
`native-builtins/src/lib.rs` at line 11604 is a mis-indented closure tail, not a
function end, and a naive scanner reads 7,000 lines of that function as
top-level.)

`java/net/URLClassLoader.findClass(Ljava/lang/String;)Ljava/lang/Class;` has
**three** registrations:

| site | body | mode |
|---|---|---|
| `native-builtins/src/classloader.rs:9593` (`register_classloader_natives`) | `ucl_find_class` | synthetic-JDK |
| `native-builtins/src/lib.rs:18747` (`register_essential_natives_with_shims`) | `classloader_real::ucl_real_find_class` | real-JDK |
| `native-builtins/src/servlet.rs:1775` (`register_s1_classloading`) | inline closure → `ensure_class_initialized` | — |

The winner was settled the only way that cannot be argued with, **in one binary
per arm**:

* The servlet closure resolves through the GLOBAL store, so if it won, group 3's
  "two loaders yield two classes" would fail. It **passes on both arms**, so
  that registration does not win in either mode.
* Both `ucl_find_class` and `ucl_real_find_class` funnel through
  `ucl_try_define_local_class`, and its message
  `URLClassLoader.findClass({name}) define failed: {msg}` — emitted at
  `classloader.rs:7715` and `:7742` and nowhere else — appeared **verbatim on
  both arms**.

So the fix does not depend on which of the two mode-specific registrars wins:
they share the body, and the body shares `find_loaded_class_for_loader` with
`findLoadedClass` (`classloader_real.rs:1189`, `:1226`; `classloader.rs:4774`),
`Class.forName` (`lang_class.rs:2758`), and eight other call sites. That is why
one edit closes both modes. `find_loaded_class_for_loader` is a plain helper,
not a registration, so last-write-wins has nothing to decide about it, and
`classloader.rs` sets no ambient `NativeKind` over the edited region.

## What changed

**`native-builtins/src/classloader.rs`, `find_loaded_class_for_loader_inner`,
the `!is_user_defined` branch — one site, and it is mode-independent.** For a
bare `URLClassLoader` the two own-loader probes the user-defined branch already
runs are consulted **first**:

```rust
if is_bare_url_class_loader(ctx, this) {
    if let Some(id) = peek_loader_namespace_id(ctx, this) {
        if let Some(cid) = ctx.class_id_defined_by_loader_exact(internal_name, id) {
            return Some(ctx.get_class_mirror(cid));
        }
    }
    if let Some(cid) = class_defined_by_this_loader_object(ctx, this, internal_name) {
        return Some(ctx.get_class_mirror(cid));
    }
}
```

Both ask "did **THIS** loader define it" — the strongest identity statement the
VM can make, and neither can return another loader's copy. A miss from either
falls through to the built-in branch **unchanged**.

**This is deliberately ADDITIVE rather than a branch flip.** Spelling the branch
predicate `is_user_defined_loader(..) || is_bare_url_class_loader(..)`, so it
matches the namespace allocator exactly, would also remove the built-in
branch's *global fallback* for a bare `URLClassLoader` — a second axis, on the
hottest reflective path in the VM, with no vector demanding it. As written, no
answer this function used to give changes; only answers it used to withhold from
a loader asking about its own class. See the named residual below.

**Mode statement.** One change, in a helper both modes share; it touches
`Compatible` and `strict` identically. Admissible under the `Compatible` freeze
as a genuine HotSpot-parity bug fix, and stated rather than assumed: HotSpot
returns the cached class for every one of these shapes, CratonVM threw for all
of them, and no compatibility layer chose any of it — the call had no working
second invocation.

**Two unit tests** beside the 2026-07-01 tests that missed this, in
`classloader_tests`: a bare `URLClassLoader` sees the class it is recorded as
having defined, and does **not** see a user-namespace class another loader
defined. The three existing built-in-loader tests instantiate
`java/lang/ClassLoader` and are untouched — they assert genuine built-in hiding,
which is correct and stays. **No test was weakened.**

## The vector

`regression-suite/src/RLoaderChurnDefine.java`, new section
`repeatLookupIsACacheHit()` (+13 checks, 1180 -> 1193). This vector already held
both halves of the duplicate-**define** rule — many loaders one name must all
succeed, one loader twice must refuse — and was missing the third: a repeated
**lookup** is a cache hit. It is a `CORE_CLASSES` vector, so it runs in both
modes and against HotSpot.

Driven through a bare `java.net.URLClassLoader` **on purpose**: every other
section here uses a `ClassLoader` subclass, and the subclass takes a different
route, which is precisely why the defect survived a vector that looks like it
covers this. URLs are the run's own `java.class.path`, so nothing is written and
the section is as deterministic as the rest.

**Anti-vacuity, stated.** Asserting the second call "did not throw" passes
against a VM that answers with a *different* `Class` object of the same name —
its own defect. Every repeat is asserted with `==`. The cross-loader half is
asserted too (two loaders, two classes, both runnable), or "return the global
copy" would pass everything else. And `probes/ForNameCacheProbe.java` group 7
keeps the **over-correction guard**: a genuine duplicate `defineClass` into one
loader must still raise `LinkageError`, so a "fix" that deletes the
duplicate-define check cannot pass.

Prove the RED, on the **pre-fix** binary at `31caacc1b`:

```
HotSpot 25.0.3   PASS RLoaderChurnDefine (1193 checks)
--jdk-only       ClassFormatError: RLoaderChurnDefine$Echo: URLClassLoader.findClass:
                 IncompatibleClassChangeError { class ... already defined by user-defined(124) loader }
--real-jdk       same
```

Reverify after the build:

```
cargo build --release -p cratonvm-cli
javac -d regression-suite/build regression-suite/src/RLoaderChurnDefine.java
target/release/cratonvm --real-jdk -cp regression-suite/build RLoaderChurnDefine   # 1193
target/release/cratonvm --jdk-only -cp regression-suite/build RLoaderChurnDefine   # 1193
java -cp regression-suite/build RLoaderChurnDefine                                 # HotSpot oracle

javac -d probes/build-aux probes/ForNameCacheProbeTarget.java
javac -d probes/build     probes/ForNameCacheProbe.java
target/release/cratonvm --real-jdk -cp probes/build ForNameCacheProbe probes/build-aux
target/release/cratonvm --jdk-only -cp probes/build ForNameCacheProbe probes/build-aux
java -cp probes/build ForNameCacheProbe probes/build-aux                           # PROBE result=OK
```

`cargo test -p cratonvm-native-builtins classloader_tests` for the two unit
tests. No `CRATONVM_*` variable was added, so nothing in `flag_groups.rs`,
`flag-surface.txt`, `flag-tokens.md` or `flag-inventory.md` moves.

## Blast radius

This is the hottest reflective path in the VM, so the direction of the change
matters more than its size.

* **Monotone.** The added probes can only turn a `None` into a `Some(the
  loader's own class)`. No value this function previously returned changes, so
  nothing that currently resolves can start resolving differently. The failure
  mode it removes is a hard `ClassFormatError`/`NoClassDefFoundError`; the
  failure mode it could introduce would require the VM to have recorded a bare
  `URLClassLoader` as the definer of a class it did not define.
* **Scope.** The new code is reached only when
  `is_bare_url_class_loader(ctx, this)` — one class-name comparison, strictly
  cheaper than the `is_user_defined_loader` call this function already makes on
  every invocation. The app, platform and boot loaders skip it.
* **Cost, honestly.** For a bare `URLClassLoader` whose namespace probe misses,
  `class_defined_by_this_loader_object` is a linear scan of the defining-loader
  store under a global mutex. That is **not a new cost shape** — every
  user-defined loader in the VM already pays exactly this on the same kind of
  miss, at `find_loaded_class_for_loader_inner`'s user-defined branch — but a
  bare `URLClassLoader` that repeatedly looks up classes it did *not* define
  (pure delegation) now joins them. Correctness first: a miss here re-drives a
  define, which is far worse than a scan.
* **Widening.** Code written around a `catch (ClassFormatError)` /
  `catch (NoClassDefFoundError)` fallback on a repeated `Class.forName` was
  taking that fallback unconditionally and will now skip it. Everything that
  resolves a class by name more than once through the same bare
  `URLClassLoader` is in range — Spring, Hibernate, Jackson, `ServiceLoader`,
  and any retry or per-request path. The old answer was manufactured; this is
  the honest direction, but it is a change.
* **Not in range:** `defineClass` (`RJdkDefineClass`, `RLoaderChurnDefine`'s
  first three sections — different road, untouched); `URLClassLoader`
  *subclasses* (already correct, measured above); the app/platform/boot loaders
  (unchanged branch); generated-proxy visibility and the
  `loader_can_see_defining` rule (both below the early return, unreached when it
  fires, and unreached exactly when the loader defined the class itself).

## Named residual, measured and deliberately not fixed here

The branch predicate is still asymmetric on its **other** half. A bare
`URLClassLoader` continues to reach the built-in branch's global fallback, so
`findLoadedClass` on one can report an application-namespace class it never
initiated — where HotSpot returns null until this loader has actually initiated
the load. Probe group 6 pins the *current* behaviour of the delegating case and
passes on all three arms, so this record does not freeze the wrong answer to a
question it is not settling.

Closing it means spelling the predicate
`is_user_defined_loader(..) || is_bare_url_class_loader(..)` and letting a bare
`URLClassLoader` take the user-defined branch outright. That is a second axis of
behaviour change across every `findLoadedClass` caller in the VM, with no vector
demanding it and no measured workload behind it. It wants its own lane, its own
oracle, and a full suite run.
