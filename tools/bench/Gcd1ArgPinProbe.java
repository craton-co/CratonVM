// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.List;

/**
 * gcd d2/f (2026-09-27): does a compiled callee's reference ARGUMENT stay
 * rooted for the whole call after the callee dropped it?
 *
 * <p>The probe the page
 * {@code docs/known-issues/gc/gengc-r5w4-jit8-call-argument-pins-outlive-the-callees-use-of-them-20260926.md}
 * specified ({@code GenR5W4ArgPinProbe}, never written). A caller passes a
 * 16 MB list, and a {@link WeakReference} to it, to
 * {@code dropAndCheck(l, box, check)}, whose body is {@code l = null; return
 * !check || cleared(ref);}. The caller keeps no copy: the list is built by
 * {@code make}, which parks the reference in a one-element array the caller
 * passes along. So at the check the list is referenced only by the argument
 * words of the call into {@code dropAndCheck} -- the interpreter's
 * {@code JitArgPinGuard} window ({@code vm/src/runtime/interpreter/jit_bridge.rs})
 * or a Rust dispatch door's {@code CompileArgPins} rerun pins
 * ({@code vm/src/jit/helpers.rs}) -- and by {@code dropAndCheck}'s own frame,
 * which nulled it.
 *
 * <ul>
 *   <li>{@code arg-pin-cold-caller}: the caller runs once (interpreted, or its
 *       first-call body).</li>
 *   <li>{@code arg-pin-warm-caller}: the caller is warmed with tiny lists
 *       first, so the call into {@code dropAndCheck} is compiled to
 *       compiled.</li>
 * </ul>
 *
 * <p>HotSpot ({@code java -XX:+UseSerialGC -Xmx64m -cp tools/bench Gcd1ArgPinProbe})
 * prints, in this order:
 * <pre>
 *   PASS arg-pin-cold-caller
 *   PASS arg-pin-warm-caller
 *   PASS all 2
 * </pre>
 * and exits 0; otherwise the summary is {@code FAIL <n> of 2} and the exit code
 * 1. The holder of a failing case is named by
 * {@code CRATONVM_DBG=oldmark-root-census,root-source}: a
 * {@code cat=.../native-pin} holder is the page's defect.
 *
 * <p>Commands:
 * <pre>
 *   javac -d tools/bench tools/bench/Gcd1ArgPinProbe.java
 *   P="--java-home $JDK -XX:+UseGenerationalGC -Xmx64m -cp tools/bench"
 *   timeout 300 cratonvm $P Gcd1ArgPinProbe
 *   timeout 300 cratonvm $P --nojit Gcd1ArgPinProbe
 *   CRATONVM_DBG=oldmark-root-census,root-source CRATONVM_DBG_JIT_ROOTSCAN=1 \
 *     timeout 600 cratonvm $P Gcd1ArgPinProbe 2>argpin.log
 * </pre>
 */
public final class Gcd1ArgPinProbe {
    static int failures;
    static long sink;

    static boolean cleared(WeakReference<?> ref) {
        for (int i = 0; i < 3 && ref.get() != null; i++) {
            System.gc();
        }
        return ref.get() == null;
    }

    static void verdict(String name, boolean ok) {
        if (!ok) {
            failures++;
        }
        System.out.println((ok ? "PASS " : "FAIL ") + name);
    }

    /** About {@code mb} megabytes of {@code long[128]} blocks; its weak reference goes to {@code box[0]}. */
    static List<long[]> make(int mb, Object[] box) {
        final List<long[]> l = new ArrayList<>();
        for (int i = 0; i < mb * 1024; i++) {
            l.add(new long[128]);
        }
        box[0] = new WeakReference<>(l);
        return l;
    }

    /** Drops its argument, then (when {@code check}) asks whether it went. */
    static boolean dropAndCheck(List<long[]> l, Object[] box, boolean check) {
        sink += l.size();
        l = null;
        @SuppressWarnings("unchecked")
        final WeakReference<List<long[]>> ref = (WeakReference<List<long[]>>) box[0];
        box[0] = null;
        return !check || cleared(ref);
    }

    /** The caller: holds nothing but the call's own argument words. */
    static boolean caller(int mb, boolean check) {
        final Object[] box = new Object[1];
        return dropAndCheck(make(mb, box), box, check);
    }

    static void guarded(String name, int mb) {
        try {
            verdict(name, caller(mb, true));
        } catch (OutOfMemoryError again) {
            verdict(name, false);
        }
    }

    public static void main(String[] args) {
        guarded("arg-pin-cold-caller", 16);
        // Warm `caller`, `make` and `dropAndCheck` with empty lists and no
        // collection; only the 16 MB call below is judged.
        for (int i = 0; i < 20_000; i++) {
            sink += caller(0, false) ? 1 : 0;
        }
        guarded("arg-pin-warm-caller", 16);
        if (failures == 0) {
            System.out.println("PASS all 2");
        } else {
            System.out.println("FAIL " + failures + " of 2");
            System.exit(1);
        }
    }
}
