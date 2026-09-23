# `NestedJarFile.close()` → `NullPointerException: ZipFile$CleanableResource.clean()` — `ZipFile.<init>`'s Bridge registration likely wins over real bytecode for a subclass's `super()` call

**Status: OPEN, root-cause hypothesis only (not confirmed by tracing actual
dispatch, not fixed).** Filed 2026-09-22 while verifying the loader/zip
residuals in
[`nonpassed-classbyclass-census-20260922.md`](nonpassed-classbyclass-census-20260922.md).
Affects `JarUrlConnectionTests` (2 of 47 tests) and `UrlJarFilesTests` (9 of
11 tests) in `spring-boot-loader`.

## Symptom

```
java.lang.NullPointerException: Cannot invoke "java.util.zip.ZipFile$CleanableResource.clean()"
	at java.util.zip.ZipFile.close(ZipFile.java:817)
	at org.springframework.boot.loader.jar.NestedJarFile.close(NestedJarFile.java:392)
	at org.springframework.boot.loader.net.protocol.jar.UrlNestedJarFile.close(UrlNestedJarFile.java:62)
```

Every failing test in both classes hits this same signature — it is one
mechanism, not several, despite the two classes' different pass/fail ratios
(`UrlJarFilesTests` fails far more of its tests because more of them open and
close a `NestedJarFile`).

## Root-cause hypothesis

`org.springframework.boot.loader.jar.NestedJarFile` is real,
ordinarily-compiled spring-boot-loader source —
`org.springframework.boot.loader.jar.NestedJarFile.java:141` is a plain
`super(file);` calling the public `java.util.zip.ZipFile(File)` constructor.
Nothing about its construction is CratonVM-fabricated.

`native-builtins/src/phases_late/zip_streams.rs:4249-4266`
(`register_p71_zip_extras`, category `NativeKind::Bridge`) registers:

```rust
let zf = "java/util/zip/ZipFile";
r.register(zf, "<init>", "(Ljava/lang/String;)V", |ctx, args| { /* sets slot 0 (name), slot 1 (closed) only */ });
r.register(zf, "<init>", "(Ljava/io/File;)V", |ctx, args| { /* same, 2-field layout */ });
```

with the comment "ZipFile = 2-field (name=0, closed=1)" — a synthetic layout
that does not allocate or link a `ZipFile$CleanableResource`, so the real
`res` field real `ZipFile.close()` reads stays at its zero-initialised
`null`. This is a `Bridge` registration, not `Intrinsic`, so per `AGENTS.md`
("real class bytes are authoritative over registered natives... only a
reviewed `Intrinsic` may win") real `ZipFile.<init>(File)` bytecode —
which does construct a real `CleanableResource` — should take priority over
it whenever real bytecode is reachable, which it always is here: `ZipFile`
is a real, unmodified JDK class.

**Not yet confirmed, but the strongest available explanation**: `NestedJarFile.<init>`'s
`super(file)` is an `invokespecial` resolved directly against
`ZipFile.<init>(Ljava/io/File;)V` — a *static*, declaring-class resolution,
unlike the virtual/interface dispatch the "real bytecode wins" rule is
usually described in terms of (`AGENTS.md`'s own examples are all
`invokevirtual`/`invokeinterface` cases: `FileSystemProvider.getScheme()`,
`DirectoryStream.iterator()`). If constructor (`<init>`) dispatch doesn't run
through the same "does the declaring class have real Code" check other
dispatch kinds do, a `Bridge`-registered `<init>` could win unconditionally,
exactly the failure shape here. This would be the same general species of bug
`9c7b4ed2e`/`907ebeb7e` (this same day, see the census page's own history)
fixed for `FileSystemProvider`/`Path` — "once `--jdk-only` prefers real
bytecode over this VM's own natives" as those commits' own message puts it —
just not yet fixed for constructors.

**Alternative not ruled out**: the synthetic 2-field layout is used
correctly for whatever originally needed it (a directly-fabricated
`ZipFile`, not reached through a real subclass's `super()`), and the actual
bug is elsewhere — a GC/layout issue that zeroes a real `res` field
post-construction, the same species as
`generational-non-moving-sweep-zeroes-a-live-filechannel-20260906.md`. Not
investigated far enough to rule this out; whoever picks this up should check
whether `res` is null immediately after construction (before any GC could
run) or only later, before assuming the hypothesis above.

## Update 2026-09-22 (same day) — the constructor-dispatch-priority hypothesis above is very likely WRONG; narrowed, not solved

Traced `resolve_dispatch`/`resolve_native_dispatch_wave1`
(`vm/src/vm/vm_exec.rs:1380-1618`) and both call sites that consult the
`java/util/zip/ZipFile` `<init>`/`close`/etc. force-list
(`native_override.rs:3140-3155`, `force_native_over_real_jdk_bytecode`):
`ClassSharedForce` (`vm_exec.rs:31008-31059`) and `ClassSharedNative`
(`vm_exec.rs:31133-31178`). **Both correctly compute `bytecode_available`
and both correctly return `None` (defer to real bytecode) for a `Bridge`
native under `--jdk-only` when bytecode is available** — `resolve_native_dispatch_wave1`'s
own logic (`vm_exec.rs:1608-1616`) is exactly the "§7 step 3" rule
`AGENTS.md` documents, doing precisely what the previous version of this
page assumed it might not: `NativeKind::Bridge if bytecode_available =>
None`. So the earlier "constructor dispatch doesn't run the same
real-bytecode-priority check" theory does not hold up against the actual
dispatch code — at least not at either of the two sites this trace covered.

This does **not** mean the force-list entry is harmless — `ZipFile.<init>`
being in it at all is still worth another look, and there may be a THIRD
call site (constructor/`invokespecial` specifically might not route through
either `ClassSharedForce` or `ClassSharedNative`; not confirmed either way)
that does get this wrong. Two directions worth checking before either is
committed to:

1. **Find the actual `invokespecial <init>` dispatch site** (search for
   where `Opcode::InvokeSpecial` or equivalent is interpreted) and confirm
   whether it goes through `resolve_native_dispatch_wave1` at all, or has its
   own native-vs-bytecode decision that does not check `bytecode_available`.
2. **GC-safety, not dispatch.** If `<init>` genuinely does run real bytecode
   and does construct a real `CleanableResource`, the null-at-`close()`
   symptom matches the OTHER known defect family in this codebase — a
   freshly-allocated field zeroed by a non-moving young-generation sweep
   before it's read (see `known-issues/springboot/
   generational-non-moving-sweep-zeroes-a-live-filechannel-20260906.md`
   and the `open_real_filechannel` GC-discipline comment this same session's
   `newByteChannel` fix added, `native-builtins/src/phases_late/nio_file.rs`).
   `ZipFile`'s real constructor allocates `res = new CleanableResource(this,
   cleaner, zsrc)` as effectively its last step — exactly the shape (a
   fresh allocation nothing else references yet) that family of bug hits.
   Confirming this needs either a GC-disabled/`-Xmx` large-heap rerun (if the
   NPE disappears, it's GC-timing) or a debug build reading `res` immediately
   after construction, before any GC-triggering call.

Whoever picks this up next: start by disproving or confirming (2), since (1)
turned out to be the less likely of the two once actually traced, not the
more likely one this page originally guessed.

## Update 2026-09-22 (third session) — ROOT CAUSE CONFIRMED: `§7 step 3` makes the ZipFile force-list inert under strict `--jdk-only`

Traced to an exact line. `vm/src/vm/vm_exec.rs::resolve_native_dispatch_wave1`
(reached from `ClassSharedForce`/`ClassSharedNative` in the same file, and
from `resolve_step1_native` in `native_override.rs` — all three dispatch
doors funnel through this one function):

```rust
if !policy.is_jdk_only() {
    // Compatible: the kind is irrelevant...
    return Some(...);
}
match kind {
    ...
    NativeKind::Bridge if bytecode_available => {
        record_native_shadows_bytecode(class_name, method_name, descriptor, kind);
        None   // <-- defers to real bytecode UNCONDITIONALLY
    }
    NativeKind::Bridge => Some(DispatchDecision::NativeBridge(callback)),
}
```

This function is only reached AFTER an earlier check
(`if !compat_native_wins { ...; return None; }`) has already confirmed
`compat_native_wins == true` — i.e. the triple genuinely IS on
`force_native_over_real_jdk_bytecode`'s explicit list, which names
`java/util/zip/ZipFile`'s `<init>`/`close`/`getInputStream`/etc. by name for
precisely this reason (that function's own comment: *"the real body
dereferences constructor state which native-backed JarFiles do not have"*).
**None of that matters once `policy.is_jdk_only()` is true**: `§7 step 3`
("concrete bytecode beats a bridge") unconditionally returns `None` for
ANY `Bridge`-kind native with `bytecode_available`, silently discarding the
force-list's verdict. `should_force_registered_native_over_bytecode`
(`native_override.rs:6159`) only changes what `compat_native_wins` IS
computed as — it has no way to make this function actually honor it once
strict policy is active.

This is not a latent bug that only fired once `--jdk-only` became default —
it is `§7 step 3`'s DESIGNED behavior, working exactly as documented, on a
force-list entry that was written assuming Compatible mode (where the early
`!policy.is_jdk_only()` return makes the force-list authoritative) and never
revisited for what strict mode does to it. The force-list comment already
named the failure mode; `--jdk-only` becoming default is what turned the
warning into a live defect.

**Why not fixed this session**: two real options, neither a narrow patch:

1. **Give `resolve_native_dispatch_wave1` a way to honor the force-list even
   under strict policy** — e.g. a new `NativeKind` (or a side-channel flag
   alongside `compat_native_wins`) for "must win even under `--jdk-only`",
   audited against every OTHER entry on the ~55-branch
   `force_native_over_real_jdk_bytecode` list to see which of THOSE are also
   silently inert the same way. Touches core VM dispatch, used by every
   native call in the process — exactly the kind of change this codebase's
   own culture (see almost any page in `docs/internal/`) insists on
   measuring broadly before landing, not something to rush through in the
   session that found it.
2. **Implement the real, low-level `ZipFile`/`ZipCoder`/`CleanableResource`
   JNI primitives** (`private static native long open(...)`, matching JDK
   25's exact `ZipFile.java` shape) so real bytecode — which `§7 step 3` is
   going to keep choosing regardless — actually works. No such native is
   registered ANYWHERE in this tree today (`grep -rn
   "\"java/util/zip/ZipFile\""` across `native-io`/`native-builtins` finds
   only the two whole-method Bridge overrides this page already covers, not
   a single JNI-shaped primitive) — a materially bigger undertaking than a
   backfill, closer in size to this same file's `SeekableByteChannel`/
   `WindowsDirectoryStream` fixes multiplied across ZipFile's entire native
   surface.

Whoever picks this up next: option 2 matches this codebase's own stated
direction better (`--jdk-only` prefers real bytecode; the natives exist to
compensate for gaps, not to override working real behavior) and doesn't risk
the other ~54 branches of the force-list the way option 1 does. Start there.

## Repro

`spring-boot-suite-runner` single-class run against `loader/spring-boot-loader`:

```powershell
run-spring-boot-suite.ps1 -Vm craton -Category all -Start 649 -Count 1 -RunName <name> -Exe <binary>
# or via run-single-class.ps1 -Module 'loader\spring-boot-loader' -ClassName org.springframework.boot.loader.net.protocol.jar.UrlJarFilesTests
```
