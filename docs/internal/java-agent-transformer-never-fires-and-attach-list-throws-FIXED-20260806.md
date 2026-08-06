# A `-javaagent` transformer was accepted and never called, and `VirtualMachine.list()` threw — FIXED 2026-08-06

**Status:** FIXED. Both rows, in both modes.

**Reproducers:** `probes/JdkOnlyPlatformProbe`'s `agent` section (via
`scripts/jdk-only-strict-probes.sh`), and `vm-cli/tests/cli_javaagent_transform.rs`
as the committed regression test.

| | HotSpot 25 | CratonVM before | CratonVM after |
|---|---|---|---|
| `premainRan()` | `true` | `true` | `true` |
| classes offered to the transformer | `positive` | **`zero`** | `positive` |
| transformer saw `JdkOnlyPlatformProbe` being defined | `true` | **`false`** | `true` |
| `com.sun.tools.attach.VirtualMachine.list()` | returns a `List` | **throws `InternalError`** | returns a `List` |

The two rows turned out to be unrelated bugs that happened to be measured by the
same probe section. They are written up separately below.

---

## Row 1 — `addTransformer` was accepted and then did nothing

### What was wrong

The per-VM transformer chain existed, `addTransformer` recorded into it
correctly, and `run_transformer_chain` walked it faithfully — but the only two
callers of that walk were `redefineClasses0` and `retransformClasses0`. **No
class-definition path consulted the chain at all.** A `ClassFileTransformer`
was therefore never offered a class being defined, not once, for any class.

`vm/src/runtime/instrument.rs` even documented the JVMTI seam as if it were
wired: `classloading`'s `install_class_file_load_hook` is defined, exported, and
called by nobody.

That is the worst shape a failure can take, and the reason this was filed at
medium-high rather than medium. `addTransformer` returns normally.
`isRetransformClassesSupported()` answers `true`. An agent has no way to detect
from inside that it will never be called — the API contract is "you will be
offered every subsequent definition". So JaCoCo, APM agents, tracing agents,
mocking agents and most profilers all reported that they had installed
successfully and then instrumented nothing, and a suite run under such an agent
produced plausible, entirely fictional output.

### Why the obvious fix does not work

The `java.lang.instrument` contract is that the transformer is handed the class
file *before it is parsed*, and the transformer is **Java code**.

Every definition path in `classloading/src/class_manager.rs` runs under the VM's
L10 class-manager write lock. Running Java under that lock deadlocks the first
time the transformer touches a class — which is immediately, because a
transformer's first call loads its own dependencies. So the hook cannot live
where the bytes are, and the bytes are not where a Java call is legal.

### The fix

Split the work across the lock boundary:

```text
  SharedVm::load_class_transformed / pre_transform_for_load   (VM crate, no lock)
       find the bytes -> run the chain -> stage the result
                                             |
  ClassManager::load_class  <----------------+  consumes the staged entry in
       place of the bytes parent delegation would have read
```

`ClassManager` gained one field, `pending_transformed_classes`, and three
methods (`find_class_bytes_for_transform`, `stage_transformed_class`,
`discard_staged_transformed_class`). `load_class` consumes a staged entry at
exactly the point it would otherwise have called `find_class_bytes_delegated`,
so the per-name loading lock, the circularity guard, the class-bytes cache and
the `ClassLoad`/`ClassPrepare` events are all unchanged and see only the final
bytes. The definition the VM installs and the definition the agent produced are
the same object by construction.

### Where the hook is called

Two funnels, chosen because they are the last points on their respective paths
that have a Java thread in hand and hold no class-manager lock:

* `resolve_class_loader_aware` — constant-pool resolution, i.e. every `new`,
  `checkcast`, `instanceof`, `anewarray`, field owner and method owner.
* `NativeContextImpl::resolve_class_loader_faithful` — `Class.forName`, JNI
  `FindClass`, and every native that resolves a class by name.

plus two more definition sites:

* `NativeContextImpl::define_class_full` — the single backend for
  `ClassLoader.defineClass1/2/0`, `Unsafe.defineClass` and
  `MethodHandles.Lookup.defineClass`, and therefore for every class a
  user-defined loader produces. Without it a `-javaagent:` would see the JDK and
  the classpath but not a single webapp / Spring / OSGi class. Hidden classes and
  redefines are excluded: hidden classes have no binding name and
  `isModifiableClass` reports them unmodifiable, and a redefine already ran the
  chain in `native_redefine_classes0`.
* `vm-cli`, for the application main class — see the ordering note below.

### Three things that are easy to get wrong here

**Supertypes.** A class's superclass and interfaces are loaded from *inside*
`define_class_shared_with_options`, under the write lock, so they are
unreachable from any hook above it. `class_file_supertypes` parses them out of
the class file and pre-*stages* them — never force-loads them, so the VM's
class-loading order is untouched and a staged supertype whose bytes are never
asked for is simply never used. Without this, a coverage agent is offered
`class Foo` and never `Foo`'s abstract base.

**The `loader` argument.** The pre-existing chain walk passed `null` for the
`ClassLoader` argument on every call. JaCoCo and most APM agents skip
`loader == null` outright, because that means a bootstrap class — so an
implementation that passed `null` for everything would satisfy
"`transformed=positive`" and still instrument nothing in the field. The load-time
path now resolves it: bootstrap is `null` (which *is* the Java-level answer, not
"unknown"), everything else gets the system class loader.
`cli_javaagent_transform.rs` pins this separately from the rewrite test.

**Re-entrancy.** A transformer is Java: it allocates, calls library code, and
loads classes — including, on its first call, its own dependencies, and
including (ByteBuddy's type pool does this) the very class it is rewriting.
`run_load_time_transform_chain` keeps a per-thread in-flight set keyed by class
name and declines a nested offer of a class already being transformed. Skipping
the nested offer costs coverage of exactly the classes the transformer pulled in
while transforming, which is also what HotSpot's own re-entrancy rules produce.

### The launcher ordering that `selfSeen=false` was pointing at

`selfSeen` was the probe's sharpest signal and it was reporting a second,
independent defect: `vm-cli` loaded the application main class *before*
`invoke_premains`. HotSpot starts its agents during VM creation and the launcher
loads the main class afterwards, which is why `premain` can instrument it. Ours
made the main class the one class a `-javaagent:` could never see, whatever the
transformer path did.

The load now happens after `invoke_premains`. The `String[]` args array moved
after both, which also closes a pre-existing window: `args_array` is a bare
`ObjectRef` on the Rust stack that no root provider knows about, and it used to
span `premain` — arbitrary Java execution, and therefore an arbitrary number of
young collections.

### Cost

One relaxed atomic load and a not-taken branch per class resolution when no
agent is installed (`ANY_TRANSFORMER_REGISTERED`, a process-global used strictly
as a negative test in front of the per-VM chain).

### Also fixed in passing

`run_chain_over_bytes` created the class-name `String` once and held it across
`alloc_byte_array` and the `transform` call, either of which can move a young
object — the native stale-local family. It is pinned now. This was a latent bug
on the pre-existing retransform path, not something the new path introduced.

---

## Row 2 — `VirtualMachine.list()` threw `InternalError`

### What it actually was

Not an attach-API gap. The class resolves, `AttachProvider.providers()` finds
`HotSpotAttachProvider`, and `listVirtualMachines` runs. It dies here:

```
java.lang.InternalError: java.lang.InternalError:
  java.lang.reflect.InvocationTargetException:
  java.lang.AbstractMethodError: method
  java/nio/file/spi/FileSystemProvider.readAttributes(Ljava/nio/file/Path;Ljava/lang/String;
  [Ljava/nio/file/LinkOption;)Ljava/util/Map; has no Code attribute
	at sun.tools.attach.HotSpotAttachProvider.listVirtualMachines(HotSpotAttachProvider.java:70)
	at com.sun.tools.attach.VirtualMachine.list(VirtualMachine.java:146)
Caused by: ...
	at sun.jvmstat.PlatformSupportImpl.<init>(PlatformSupportImpl.java:51)
	at java.nio.file.Files.getAttribute(Files.java:1822)
	at java.nio.file.Files.readAttributes(Files.java:1917)
```

jvmstat's container detection reads `unix:dev` on the temp directory. Two frames
below that, `readAttributes(Path, String, LinkOption...)` resolved to the
**abstract declaration** on `java.nio.file.spi.FileSystemProvider`.

In the real JDK that method's body lives on `sun.nio.fs.AbstractFileSystemProvider`
and every concrete provider inherits it. CratonVM's default-filesystem provider
object is stamped with the abstract `java.nio.file.spi.FileSystemProvider` — the
concrete name (`sun.nio.fs.UnixFileSystemProvider`) is only a `getClass()`
display remap in `native-builtins/src/lib.rs`, not the object's real class. So
there was nothing to dispatch to.

`probes/NioAttrProbe`, before the fix:

```text
HotSpot   unix:dev = 66305           basic:* = 9 keys
CratonVM  AbstractMethodError        AbstractMethodError
```

That is every `Files.readAttributes(p, "basic:*")` and every
`Files.getAttribute` on the platform filesystem, not an attach-API corner.

### Two more defects behind it

* `Files.readAttributes(Path, String, LinkOption...)` had a registration that
  returned an **empty HashMap for every call**. Every caller reads the result
  with `map.get(name)`, so an empty map is worse than an exception: every
  attribute of every file read back as `null`, arbitrarily far from here. (It
  was unreachable in real-JDK mode, where the real `Files` bytecode wins — so
  the lie was confined to synthetic-JDK mode, where nothing had exercised it.)
* `Files.getAttribute(Path, String, LinkOption...)` had no registration at all.

### The fix

`read_named_attributes` in `native-builtins/src/phases_late/nio_file.rs` serves
all three from one implementation, with the JDK's error contract rather than a
degraded answer:

* unknown view → `UnsupportedOperationException`
* unknown attribute name → `IllegalArgumentException`
* missing file → `NoSuchFileException`

Views `basic`, `posix`, `unix`, `dos`, `owner`. The per-view name table is
shared between the `*` expansion and the by-name lookup, so the two cannot
disagree — that mismatch is exactly how "`*` returned it but asking for it by
name threw" bugs happen. `owner`/`group` delegate to the same attribute object
`p59_files_read_attributes` already builds, so principals stay one
implementation.

---

## What the fix does *not* cover

* **Classes an agent registers a transformer too late to see.** Classes loaded
  before `premain` runs (the JDK bootstrap set) are already defined;
  `retransformClasses` is the API for those, and it was already wired.
* **`setNativeMethodPrefix`.** Still `false` from
  `isNativeMethodPrefixSupported0`, deliberately — see the standing warning on
  that function. Nothing consults `TransformerEntry::native_method_prefix` at
  native dispatch, and answering `true` would be an unbacked capability claim of
  the same shape as this bug.
* **`VirtualMachine.list()` returning a non-empty list.** It returns a `List`,
  which is what the contract and the probe require; whether it *finds* anything
  depends on `hsperfdata` files CratonVM does not write.

## Related memory

`native-backed-state-is-invisible-to-real-jdk-bytecode`,
`synthetic-standin-checkcast-use-jdk-interfaces` — Row 2 is another instance of
the same shape: a native-backed stand-in object stamped with an abstract or
interface type, met by real JDK bytecode that expects a concrete subclass.
