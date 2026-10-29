> **j27 wave 5 (lane REFLECT, 2026-10-28): FIXED IN CODE, PENDING HOST VERIFICATION.** Cause: the error is
> raised INSIDE the `getDeclaringClass0` native (`native_class_get_declaring_class` -> `throw_jca_exc`, both
> the member arm and the local-class arm), but CratonVM calls natives without a Java frame, and the one-frame
> rule that puts a native's `(Native Method)` frame back on top (`stackwalker::native_leaf_frame`) refused the
> whole `IncompatibleClassChangeError` / `NoClassDefFoundError` family as "raised by the invoke before entering"
> (`native_leaf_throwable_admitted`). For `Class`'s private nesting natives the invoke can raise neither (boot
> class, private method: nothing to resolve), so a new family `stackwalker::class_nesting_native_raised`
> admits ICCE and NCDFE for `getDeclaringClass0()Ljava/lang/Class;` and `getDeclaredClasses0()[Ljava/lang/Class;`
> (HotSpot's `JVM_GetDeclaringClass` / `JVM_GetDeclaredClasses` run `check_for_inner_class` and `klass_at`
> there); `getSimpleBinaryName0` is deliberately not in it (`find_inner_classes_attr` runs no check). The four
> admission rules now sit in one helper, `native_leaf_raise_admitted`. Parity fix, no switch (the existing
> `CRATONVM_THROWABLE_NATIVE_LEAF_FRAMES=0` still refuses every native leaf frame). Both modes (the rule is in
> the trace capture, not a mode arm). Unit tests: `vm` crate,
> `runtime::stackwalker::j27_class_nesting_native_leaf_tests::{nesting_natives_list_their_frame_for_their_linkage_errors,
> other_linkage_errors_and_natives_keep_the_caller_on_top}`. Probe: `tools/probes/jit/j27/J27ReflectNestingFrames.java`
> (expected HotSpot 25 lines in its header; `nest-declaring-class`, `nest-simple-name`, `nest-*-top`, a 20 000-call
> hot row for a compiled innermost frame, a consistent-pair control). Verify: `cargo test -p cratonvm-vm --lib
> j27_class_nesting_native_leaf -j 5`; then the probe and `J27Mh2Rows` (`mh717-2-wmte-spoofed`,
> `mh717-2-simple-name` must now start `@ java.lang.Class.getDeclaringClass0 | java.lang.Class.isTopLevelClass`)
> diffed against HotSpot 25 in `--jdk-only` and `--compatible`. Other natives the page asked about: `forName0`
> has its own family (`callback_native_leaf_frames`), `Array.newArray`'s `NegativeArraySizeException` /
> `IllegalArgumentException` pass the general rule; not changed. Not covered: the `--compatible` name /
> enclosing-class bridges that raise the same ICCE without running the JDK body
> (`lang_class::declaring_class_icce`, `member_class_inner_classes_disagreement`,
> `name_bridge_outer_resolution_error`) list neither the native nor the JDK chain; if the probe's
> `nest-enclosing-class-top` / `nest-simple-name` rows differ only under `--compatible`, that is those bridges
> (file a page; the fix is a stand-in chain or retiring the bridge, AGENTS.md "real class bytes are authoritative").
> HotSpot's `klass_at` can also raise `IllegalAccessError` (an ICCE subclass) there; CratonVM never raises it
> from these natives, so it is not admitted.

# j27: the spoofed-member ICCE has no `Class.getDeclaringClass0` frame

| | |
|---|---|
| **Status** | OPEN. Filed 2026-10-28 by j27 wave 4 lane FRAMEAB (found while running the j27 fact battery; not worked). |
| **Area** | native-builtins `classloader.rs` / `lang_class.rs` (spoofed-member `IncompatibleClassChangeError`), exception stack traces of natives. |
| **Probe** | `tools/probes/jit/j27/J27Mh2Rows.java`, rows `mh717-2-wmte-spoofed`, `mh717-2-simple-name` |

## Evidence

Both modes, with or without the JIT frame switches (a local Windows build of the j27 worktree at
`d101ea13a0` plus the working tree). The message matches HotSpot 25, the stack does not. HotSpot 25's top
frame is the native method that raises the error:

    fact mh717-2-simple-name java.lang.IncompatibleClassChangeError: j27mh2.O1 and j27mh2.G1 disagree on InnerClasses attribute @ java.lang.Class.getDeclaringClass0 | java.lang.Class.isTopLevelClass | ...

CratonVM starts the trace at the caller:

    fact mh717-2-simple-name java.lang.IncompatibleClassChangeError: j27mh2.O1 and j27mh2.G1 disagree on InnerClasses attribute @ java.lang.Class.isTopLevelClass | ...

The `mh717-2-wmte-spoofed` row (the message built in `Invokers.newWrongMethodTypeException`) has the same
missing frame.

## What would fix it

HotSpot fills the stack trace while the native frame of `getDeclaringClass0` is on the stack, so a
`Throwable` raised by a JNI or VM native lists that native first. Find where CratonVM raises this ICCE
(`spoofed_member_disagreement` / `simple_name_inner_classes_disagreement` and the `getDeclaringClass0`
native, `compute_enclosing_class`). Either the error is raised before the native frame is pushed (an
intercept in front of `isTopLevelClass`), or the backtrace fill skips native frames. Check which, then
check whether other natives that throw have the same gap (e.g. `Class.forName0`, `Array.newArray`), against
HotSpot.
