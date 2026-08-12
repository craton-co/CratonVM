# W7-79 — `Runtime.load0`/`loadLibrary0` read the wrong argument, on the `Compatible` arm too

**Status: FIXED 2026-08-12, with a vector.** The `Compatible` arm now decodes
its argument vector through `runtime_load_args` like the strict arm has since
2026-08-11. `RJdkJni` grew five checks (35 -> 40) that fail on the pre-fix
binary in `--real-jdk` and pass in `--jdk-only`, so the fix is not one more
entry in this directory's pile of patches that were already applied.

Branch `fix/runtime-loadlibrary-compatible-arm-20260812`, off dev `87809196b`.

**IN THE TREE — re-read 2026-08-12, second pass, not taken from this record.**
Rule 1 of the campaign README applies to a record claiming its own fix landed
just as much as to one claiming a fix did not, and this directory has handed the
same work out twice before. Checked by reading the fork, not by grepping for a
token: `native-builtins/src/lang_system.rs` calls `runtime_load_args` at
**`:1522`** (`loadLibrary0`, strict), **`:1540`** (`load0`, strict),
**`:1596`** (`loadLibrary0`, `Compatible`) and **`:1615`** (`load0`,
`Compatible`); the helper is at **`:1717`** and takes the name from
`args.get(2)` (`:1722`), the `fromClass` mirror from `args.get(1)` (`:1718`).
Not one `args.get(1)`-as-name read is left on either arm, and the `Compatible`
bodies carry the two `args[2]` comments this record's patch specified.
`LoaderScoping::Off` is intact on both `Compatible` bodies (`:1605`, `:1622`),
so the strict-only cross-loader rule did not leak with the fix. **Nothing to
apply. Do not re-hand this out.**

**THIRD PASS 2026-08-12 (lane A14) — still in the tree, line numbers now stale.**
Re-checked by reading the fork, not by grepping for a token, and the structure
holds exactly as the block above describes: four call sites, two per arm, all
decoding through `runtime_load_args`; the `Compatible` bodies still carry the
two `args[2]` comments; `LoaderScoping::Off` still on both `Compatible` bodies
and `LoaderScoping::On` on the strict ones. **Every line number in the block
above has drifted** — the helper is now at `:1830` (not `:1717`), the strict
sites at `:1635`/`:1653` and the `Compatible` sites at `:1709`/`:1728`. The
symbols are the durable reference. **Still nothing to apply. Do not re-hand
this out.** See the new section at the end for the one arming question that IS
still open on this road, and why it must not be settled on Windows.

**Vector count moved 40 -> 41 on 2026-08-12** by W5-1's second pass, which
asserted *which* library `libraryLoading()` ends up with rather than only that
one loaded. The five checks below are unaffected and still sit where this record
put them; the "Prove-the-RED" transcript further down quotes 40 and is a
historical measurement, not a current expectation.

## The recorded diagnosis was checked before it was believed, and it holds

This directory has produced nine records whose prescribed fix was wrong while
the observation was right, so the index was **measured**, not read off a
`javap` listing. One binary — `target/release/cratonvm` at dev `87809196b`,
which is this branch's own base — HotSpot 25.0.3 beside it, and the mode flag
as the only variable:

```java
try { Runtime.getRuntime().loadLibrary("cratonvm_probe_zzz"); }
catch (UnsatisfiedLinkError e) { print(e.getMessage()); }
try { Runtime.getRuntime().loadLibrary("net"); print("LOADED"); }
catch (UnsatisfiedLinkError e) { print(e.getMessage()); }
```

| arm | missing library | `net` |
|---|---|---|
| HotSpot 25.0.3 | `no cratonvm_probe_zzz in java.library.path: <path>` | LOADED |
| `--jdk-only` (reads `args[2]`) | `no cratonvm_probe_zzz in java.library.path` | LOADED |
| `--real-jdk` (read `args[1]`) | `no  in java.library.path` | `no  in java.library.path` |

The `Compatible` arm named **nothing** — the double space is the whole
signature — and failed for a library HotSpot loads. The strict arm, which
differs from it in exactly this one decode, names the library and loads `net`.
That is the discriminator, and it is the reason the defect survived: **a probe
that asserts only "it threw" passes against either index**, because reading the
`Class` mirror as a name yields the empty string and an empty name is not on
`java.library.path` either.

So `args[0]` is the `Runtime` receiver, `args[1]` the `fromClass` mirror, and
`args[2]` the name — the convention `Runtime.addShutdownHook` forty lines above
these registrations already states, now confirmed against a running VM rather
than against a comment.

`Runtime.load(String)` is the same two lines and was measured with it:

| arm | `Runtime.load(<absent absolute path>)` |
|---|---|
| HotSpot | `Can't load library: C:\…\cratonvm-probe-no-such.dll` |
| `--jdk-only` | `no C:\…\cratonvm-probe-no-such.dll in java.library.path` |
| `--real-jdk` | `no  in java.library.path` |

Two divergences, not one, and they are independent — see the named residual at
the end for the message *shape*.

## Which registration wins

Grepped, then read; not brace-scanned.

* `java/lang/Runtime.{load0,loadLibrary0}(Ljava/lang/Class;Ljava/lang/String;)V`
  and `java/lang/System.{load,loadLibrary}(Ljava/lang/String;)V` are registered
  in **exactly one place**: the compatibility-mode fork in
  `native-builtins/src/lang_system.rs`. Each triple is registered once per
  registry, in one arm of the fork or the other, so there is no self-shadow row
  and nothing for last-write-wins to decide.
* The only other mentions are non-registrations: `native-api/src/capability.rs`
  classifies them as `CapabilityKind::LibraryLoad`; `native-api/src/registry.rs`
  uses the names in registry unit tests with dummy callbacks; and
  `native-builtins/src/jmx.rs:7737-7757` is a **test that asserts
  `register_vm_management_impl` does NOT register any of the four**, because it
  runs after `lang_system` in both modes and a registration there would shadow
  the real loader. That hazard is already ratcheted.
* `lang_system.rs` contains no `set_category`/`with_category` at all, so no
  ambient `NativeKind` block spans the fork.
* The winner was also confirmed the only way that cannot be argued with: the two
  arms of the fork produce the two different messages in the table above, in one
  binary, selected by the mode flag.

`vm_exec.rs` (the RKC16N.12 clause) forces the native override for
`java/lang/System.{load,loadLibrary}` and `java/lang/Runtime.{load0,load,
loadLibrary0,loadLibrary}`, so the real `ClassLoader.loadLibrary` bytecode never
runs for any of them. That matters for W6-6's road; see below.

## What changed

`native-builtins/src/lang_system.rs`, the `else` (Compatible) arm only. Each of
the two `Runtime` bodies replaces its `match args.get(1)` prologue with the one
call the strict arm makes:

```rust
let (_from_class, name) = runtime_load_args(&*ctx, args);
```

`LoaderScoping::Off` is **unchanged**, so the cross-loader `loadedLibraryNames`
rule stays strict-only. `_from_class` is discarded rather than threaded because
`loaded_by` early-returns on `Off` before it is read; discarding it keeps the
difference between the two arms exactly one axis wide.

## Why this is admissible under the `Compatible` freeze

`Compatible` is frozen except for genuine HotSpot-parity bug fixes, and this is
stated explicitly rather than assumed: `Runtime.getRuntime().loadLibrary(x)`
failed for **every** `x`, including libraries the JDK image ships and HotSpot
loads. Nothing about reading the wrong argument index is a compatibility-layer
substitution — there is no behaviour here that a compatibility layer chose. The
method had no working input.

## Blast radius, and what could depend on the broken behaviour

The change is a **behaviour widening**: a call that always threw
`UnsatisfiedLinkError` can now succeed. Anything written around a
`catch (UnsatisfiedLinkError)` fallback was taking that fallback
unconditionally and will now sometimes skip it. Concretely:

* **Netty `NativeLibraryLoader`, Tomcat `AprLifecycleListener`/tcnative,
  Elasticsearch's native access** — all probe with a load and fall back to pure
  Java. They reach loading through `System.loadLibrary`, which was already
  correct, so they are **not** in range. The `Runtime` spelling is the rare one.
* **A caller that used `Runtime.getRuntime().loadLibrary(...)` as a
  "does this VM have native X" test** and relied on the answer being no. Such a
  caller now gets a real answer, which may be yes for anything on the
  `is_vm_provided_jdk_library` allowlist or genuinely on `java.library.path`.
  This is the honest direction of the two — the old answer was manufactured —
  but it is a change.
* **A caller that loads a real third-party JNI library through `Runtime`** now
  actually opens it, so `JNI_OnLoad` runs where it previously did not. Any
  side effect of that library's initialisation is newly live.
* Not in range: `System.load`/`System.loadLibrary` (unchanged, both arms),
  the cross-loader rule (`LoaderScoping::Off` on this arm, unchanged), and
  `jdk/internal/loader/NativeLibraries.load` (different road, W6-6's, unchanged).

No revert knob was added. The `Compatible` arm is now byte-identical in
behaviour to the strict arm on these two triples except for `LoaderScoping`,
so a bisect has the mode flag itself as its A/B.

## The vector

`regression-suite/src/RJdkJni.java`, `libraryLoading()`, five new checks
(35 -> 40). It is the shared vector, so it must pass in **both** modes and
match HotSpot; the cross-loader behaviour below is deliberately NOT asserted
there because it legitimately differs between the two modes.

1. `Runtime.getRuntime().loadLibrary(<name that cannot exist>)` throws, and
2. **the message CONTAINS that name.** This is the assertion the old vector
   could not have made — the message text is still never printed, only the
   predicate, because it embeds `java.library.path`.
3. `Runtime.getRuntime().load(<absent absolute path>)` throws, and
4. **the message CONTAINS that path.** Both HotSpot's `Can't load library: <p>`
   and this VM's `no <p> in java.library.path` contain it, so this discriminates
   the argument index without freezing the message shape the two still disagree
   on.
5. Whatever `System.loadLibrary` just loaded must also load through `Runtime` —
   the positive direction, which no "it threw" assertion can reach. Same VM,
   same loader, so the JDK answers out of this loader's own cache and the
   cross-loader error is not in range.

Prove-the-RED, on the **pre-fix** binary at `87809196b`, one binary, one flag:

```
HotSpot 25.0.3      PASS RJdkJni (40 checks)
--jdk-only          PASS RJdkJni (40 checks)      # already read args[2]
--real-jdk          AssertionError: Runtime.loadLibrary's UnsatisfiedLinkError
                    must name the library asked for
```

All five `CK RJdkJni …` lines are byte-identical to HotSpot on the strict arm,
including `CK RJdkJni loadedLibrary=net mapped=foo.dll` — the `net` fallback
W5-1 opened for still holds after the extension.

Reverify after the build:

```
cargo build --release -p cratonvm-cli
javac -d regression-suite/build regression-suite/src/RJdkJni.java
target/release/cratonvm --real-jdk  -cp regression-suite/build RJdkJni   # must reach 41
target/release/cratonvm --jdk-only  -cp regression-suite/build RJdkJni   # must reach 41
java -cp regression-suite/build RJdkJni                                   # HotSpot oracle
```

## The cross-loader measurement, kept because it settles two other records

Not a `RJdkJni` assertion — it diverges by mode on purpose — but it is the run
W5-1 said it needed. Two `URLClassLoader`s over one directory, each loading its
own copy of a class that loads `sunmscapi` (self-contained on Windows, so the
real `LoadLibraryW` succeeds):

| | HotSpot | `--jdk-only` | `--real-jdk` |
|---|---|---|---|
| loader1 | LOADED | LOADED | LOADED |
| loader1 again | LOADED | LOADED | LOADED |
| loader2 | `Native Library <java.home>\bin\sunmscapi.dll already loaded in another classloader` | `Native Library sunmscapi already loaded in another classloader` | LOADED |

Identical through the `Runtime` road on the strict arm, which also exercises
`requesting_loader_id`'s `fromClass` path end to end. On the pre-fix
`Compatible` arm the same `Runtime` run answered `no  in java.library.path`
three times — the defect this record fixes, seen from the other side.

Reproduction (`aux/` holds only `LibProbe.class`, off the application
classpath):

```java
public class LibProbe {
    public static String go(String lib) {
        try { System.loadLibrary(lib); return "LOADED"; }
        catch (UnsatisfiedLinkError e) { return "ULE[" + e.getMessage() + "]"; }
    }
    public static String goRt(String lib) {
        try { Runtime.getRuntime().loadLibrary(lib); return "LOADED"; }
        catch (UnsatisfiedLinkError e) { return "ULE[" + e.getMessage() + "]"; }
    }
}
// main: two `new URLClassLoader(name, new URL[]{aux}, null)`, one
// `Class.forName("LibProbe", true, cl)` EACH — see the trap below — then
// invoke `go`/`goRt` on loader1 twice and loader2 once.
```

**Trap, and a defect found in passing.** Calling `Class.forName("LibProbe",
true, l1)` a **second time** on the same loader fails on CratonVM with
`IncompatibleClassChangeError: class LibProbe already defined by
user-defined(3) loader`, surfaced as a `ClassFormatError` out of
`URLClassLoader.findClass`. HotSpot returns the already-defined class. A repeat
`Class.forName` on one loader is not a redefinition, and this is unrelated to
library loading — resolve each loader's `Class` object once and cache it, or
the probe dies before it reaches the interesting line. Out of this lane; not
filed here beyond this paragraph, and it is the reason the first version of
this probe reported "loader2 CANNOT DEFINE".

**Picked up and FIXED 2026-08-12** — see W7-82-forname-duplicate-define.md.
The observation above is exactly right, and the cause turned out to be one rung
earlier than a redefinition: `java/net/URLClassLoader` is on
`is_builtin_loader_class`'s list, so a **bare** instance (a subclass is fine)
was classified as a built-in LOADER and could not see the class it had itself
defined. The lookup then re-drove the define, which the duplicate rule
correctly refused. Note for anyone re-running this record's cross-loader table:
the caching workaround above is no longer required.

## Named residual, newly measured: `System.load`/`Runtime.load` message SHAPE

Independent of the argument index, and it is a **both-modes, mode-independent**
divergence:

```
HotSpot     Can't load library: C:\…\cratonvm-probe-no-such.dll
CratonVM    no C:\…\cratonvm-probe-no-such.dll in java.library.path
```

`lang_system::load_library_or_throw` emits one message for both spellings, but
HotSpot has two: `no <name> in java.library.path: <path>` for a bare name that
was searched for, and `Can't load library: <path>` for an absolute path that was
named outright. This VM's own other road already gets it right — the
`NativeLibraries.load` body in `native-builtins/src/lib.rs` formats
`Can't load library: {name}` — so the two roads disagree with each other as well
as with HotSpot.

Deliberately **not** fixed here. It is a second axis of `Compatible` behaviour
change with no vector demanding it, and closing it means splitting the message
on `LibrarySpelling` in `load_library_or_throw`, which moves `System.load` too.
The extended `RJdkJni` asserts only that the message CONTAINS the path, which
holds on both shapes, so this record does not freeze the wrong one.

The `java.library.path` **suffix** HotSpot appends to the bare-name message is
also absent here. Same function, same one-line fix, same reason for not taking
it in this lane.

---

## The one thing still open on this road: the `BootLoader.loadLibrary` arming, which MUST be A/B'd on Linux

Added 2026-08-12 (lane A14). Not a defect in this record's two triples — it is
the adjacent, deliberately-unarmed half of the same loader-scoping feature, and
it is recorded here so the next lane on this road does not "fix" it into an
unmeasured state.

**What is true today, read from source:**

* `jdk/internal/loader/BootLoader.loadLibrary(Ljava/lang/String;)V` is
  registered in `native-builtins/src/lib.rs` as a bare
  `|_ctx, _args| Ok(None)` — a deliberate no-op, with a `KEEP` comment.
* `lang_system::record_boot_loader_library` exists, is `pub`, and has
  **zero callers in the workspace**. Its own doc comment says so outright:
  *"THIS HAS NO CALLER IN THE TREE. It is a written-down hand-off, not a live
  path — do not read its presence as the feature being on."* The exact patch
  that would arm it is written out inside that comment.
* So the boot loader never claims a library, `LOADED_LIBRARIES` never records
  loader id 0, and the cross-loader rule cannot fire for anything the boot
  loader loaded. Under `Compatible` the recording would be inert anyway —
  nothing reads `LOADED_LIBRARIES` without a `LoaderScoping::On` registration,
  and only the strict arm installs one.

**Why the hold is not caution, and why Windows cannot settle it.** The no-op is
load-bearing for a platform-specific reason that is stated at both sites: Linux
real-JDK boot classes such as `java.net.NetworkInterface` call
`BootLoader.loadLibrary("net")` during `<clinit>`, and the JDK bytecode can
block indefinitely acquiring the native-library lock before it reaches the
non-fatal fallback. The neighbouring `NativeLibraries.load` registration carries
the same finding from the other direction — *"on Windows the classes exercised
so far apparently resolve via a different bootstrap route, but on Linux real-JDK
static init … calls this directly"*.

That is the whole argument: **on Windows this road is inert.** A Windows A/B of
the arming would exercise a path the boot classes do not take here, come back
green, and mean nothing — the "a narrow probe reports its own reach, not the
defect" failure, with the added trap that the green looks like evidence *for*
arming.

**And the arming is not free.** Arming it makes `System.loadLibrary("net")`
throw for any program that has already reached a `java.net` boot class. That is
HotSpot's answer, and it is exactly what `RJdkJni.libraryLoading` depends on NOT
happening: its `zip` probe falls through to a `net` probe that must succeed, and
`run.sh` compares `CK` lines. Whether CratonVM reaches
`BootLoader.loadLibrary("net")` before that line **cannot be settled from
source**.

**Disposition: leave unarmed. The decision belongs to a Linux run.** What that
run needs, and it is one binary with the mode flag as the only variable:

1. On **Linux**, both modes, with the HotSpot 25 oracle beside them, confirm
   whether `BootLoader.loadLibrary("net")` is reached before
   `RJdkJni.libraryLoading`'s `net` probe.
2. If it is not reached, arming is a no-op on the vector and the patch in
   `record_boot_loader_library`'s doc comment can land with a green
   `RJdkJni`.
3. If it is reached, arming moves `RJdkJni` and the vector has to change with
   it in the same commit — and that is a `Compatible`-behaviour change needing
   its own justification under the freeze, since HotSpot parity here means the
   `net` probe starts throwing.

Nothing about this was measured by this lane. It is recorded as a stated hold
with its reason, which is the state this record found it in and the state it
should stay in until somebody has the Linux arm.
