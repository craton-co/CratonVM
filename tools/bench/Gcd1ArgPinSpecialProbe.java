// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.WeakReference;

/**
 * gcd d10/f (2026-09-28): does a warmed caller keep a rooted copy of its
 * reference ARGUMENT for the whole call, at a direct call site whose caller
 * commits nothing before the call?
 *
 * <p>Row 2 of
 * {@code docs/internal/gc/gcd-d9d-call-argument-copies-still-rooted-on-other-call-shapes-FIXED-20261012.md}
 * and the hand-off page
 * {@code docs/internal/gc/gcd-d10f-owned-args-handoff-replays-the-caller-unchecked-DONE-20260928.md}.
 * A single-pass direct call keeps no copy of its arguments
 * ({@code CRATONVM_JIT_CALLEE_OWNED_ARGS}) only where handing a rare callee
 * trap to the caller's caller -- a re-run of the caller from entry -- commits
 * nothing twice: the caller's bytecode before the call (closed over loops) has
 * no store, no invoke and no monitor operation. Each caller here only
 * allocates the argument ({@code newarray}) and calls; the 16 MB
 * {@code long[]} is referenced by nothing else, and the callee wraps it in a
 * {@link WeakReference}, drops it and polls the reference through
 * {@code System.gc()}:
 *
 * <ul>
 *   <li>{@code super-special}: {@code super.drop(..)} -- an
 *       {@code invokespecial} (invoke kind 1);</li>
 *   <li>{@code private-special}: a private instance method ({@code javac} 25
 *       emits {@code invokevirtual}, which the single-pass planner pins to
 *       kind 1);</li>
 *   <li>{@code static-direct}: an {@code invokestatic} (kind 3, d9/d's
 *       shape).</li>
 * </ul>
 *
 * <p>{@code Gcd1ArgPinProbe}'s warm caller is the other side of the rule: it
 * calls {@code make} before the owned call, so its site keeps the copy.
 *
 * <p>HotSpot ({@code java -XX:+UseSerialGC -Xmx64m -cp tools/bench Gcd1ArgPinSpecialProbe},
 * and {@code -Xint}) prints, in this order:
 * <pre>
 *   PASS super-special
 *   PASS private-special
 *   PASS static-direct
 *   PASS all 3
 * </pre>
 * and exits 0; otherwise the summary is {@code FAIL <n> of 3} and the exit code
 * 1. gcf f4/f: with the argument {@code more} three more shapes follow --
 * {@code virtual-direct} and {@code interface-direct} (d9d row 2, kinds 0 and
 * 2) and {@code spliced-direct} (row 4) -- and the summary reads
 * {@code PASS all 6} / {@code FAIL <n> of 6}. The holder of a failing case is named by
 * {@code CRATONVM_DBG=oldmark-root-census,root-source}.
 *
 * <p>The rule is the SINGLE-PASS tier's. A caller the optimizing tier
 * recompiles (hot callers are superseded in the background) takes that tier's
 * direct call, which keeps its argument homes live across the call (row 5 of
 * the page), so the gating arm switches the IR tier's call lowering off and
 * keeps every caller here single-pass; the default arm is reported, not
 * gated.
 *
 * <p>Commands:
 * <pre>
 *   javac -d tools/bench tools/bench/Gcd1ArgPinSpecialProbe.java
 *   P="--java-home $JDK -XX:+UseGenerationalGC -Xmx64m -cp tools/bench"
 *   SP="CRATONVM_JIT_IR_CALL=0 CRATONVM_JIT_IR_CALL_SPECIAL=0 CRATONVM_JIT_IR_CALL_VIRTUAL=0"
 *   env $SP timeout 300 cratonvm $P Gcd1ArgPinSpecialProbe
 *   env $SP CRATONVM_JIT_CALLEE_OWNED_ARGS=0 timeout 300 cratonvm $P Gcd1ArgPinSpecialProbe
 *   timeout 300 cratonvm $P Gcd1ArgPinSpecialProbe
 * </pre>
 */
public final class Gcd1ArgPinSpecialProbe {
    /** 16 MB of {@code long}. */
    static final int BIG = 2 * 1024 * 1024;

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

    /** Wraps its argument weakly, drops it, then (when {@code check}) asks whether it went. */
    static boolean dropStatic(long[] a, boolean check) {
        final WeakReference<long[]> ref = new WeakReference<>(a);
        sink += a.length;
        a = null;
        return !check || cleared(ref);
    }

    /** The base whose method the subclass calls through {@code super}. */
    static class Base {
        boolean drop(long[] a, boolean check) {
            final WeakReference<long[]> ref = new WeakReference<>(a);
            sink += a.length;
            a = null;
            return !check || cleared(ref);
        }
    }

    static final class Derived extends Base {
        @Override
        boolean drop(long[] a, boolean check) {
            // Never called by the probe: only `super.drop` is.
            return false;
        }

        /** {@code super-special}: allocates the argument and calls, nothing else. */
        boolean superCaller(int n, boolean check) {
            return super.drop(new long[n], check);
        }

        private boolean dropPrivate(long[] a, boolean check) {
            final WeakReference<long[]> ref = new WeakReference<>(a);
            sink += a.length;
            a = null;
            return !check || cleared(ref);
        }

        /** {@code private-special}: the private twin. */
        boolean privateCaller(int n, boolean check) {
            return dropPrivate(new long[n], check);
        }
    }

    static final Derived DERIVED = new Derived();

    // ------------------------------------------------------------------
    // gcf f4/f: the shapes of d9d rows 2 (kinds 0/2) and 4, run only with the
    // argument `more` (the default output above is unchanged). Each caller
    // again only reads a static, allocates the argument and calls.
    // ------------------------------------------------------------------

    /** The interface of {@code interface-direct} (invoke kind 2). */
    interface Dropper {
        boolean dropIface(long[] a, boolean check);
    }

    /**
     * Not final, never subclassed: {@code dropVirtual} is a monomorphic
     * {@code invokevirtual} (kind 0) and {@code dropIface} a monomorphic
     * {@code invokeinterface} (kind 2) -- the single-pass direct instance
     * call whose service reads the receiver's class out of its argument copy.
     */
    static class Leaf implements Dropper {
        boolean dropVirtual(long[] a, boolean check) {
            final WeakReference<long[]> ref = new WeakReference<>(a);
            sink += a.length;
            a = null;
            return !check || cleared(ref);
        }

        @Override
        public boolean dropIface(long[] a, boolean check) {
            final WeakReference<long[]> ref = new WeakReference<>(a);
            sink += a.length;
            a = null;
            return !check || cleared(ref);
        }
    }

    static final Leaf LEAF = new Leaf();
    static final Dropper DROPPER = LEAF;

    /** {@code virtual-direct}: row 2, kind 0. */
    static boolean virtualCaller(int n, boolean check) {
        return LEAF.dropVirtual(new long[n], check);
    }

    /** {@code interface-direct}: row 2, kind 2. */
    static boolean interfaceCaller(int n, boolean check) {
        return DROPPER.dropIface(new long[n], check);
    }

    /**
     * Small and static, so a single-pass caller splices it; its call to
     * {@code dropStatic} is then a direct call INSIDE a spliced body, with
     * the splice's own argument copy (row 4).
     */
    static boolean relay(long[] a, boolean check) {
        return dropStatic(a, check);
    }

    /** {@code spliced-direct}: row 4. */
    static boolean splicedCaller(int n, boolean check) {
        return relay(new long[n], check);
    }

    /** {@code static-direct}: the static twin. */
    static boolean staticCaller(int n, boolean check) {
        return dropStatic(new long[n], check);
    }

    static boolean run(int shape) {
        switch (shape) {
            case 0:
                return DERIVED.superCaller(BIG, true);
            case 1:
                return DERIVED.privateCaller(BIG, true);
            case 3:
                return virtualCaller(BIG, true);
            case 4:
                return interfaceCaller(BIG, true);
            case 5:
                return splicedCaller(BIG, true);
            default:
                return staticCaller(BIG, true);
        }
    }

    static void guarded(String name, int shape) {
        try {
            verdict(name, run(shape));
        } catch (OutOfMemoryError again) {
            verdict(name, false);
        }
    }

    public static void main(String[] args) {
        // gcf f4/f: `more` adds the row-2 (kinds 0/2) and row-4 shapes.
        final boolean more = args.length > 0 && args[0].equals("more");
        // gcg g3/v: `settle` (second argument) lets background compiles land
        // before the judged calls -- a pause, then a second warm-up -- so a
        // line that FAILs only without it was judged with an INTERPRETED
        // caller (d9d row 6, the interpreter's argument pins), and a line
        // that FAILs either way is a compiled caller's door (row 3). Stdout
        // is unchanged.
        final boolean settle = args.length > 1 && args[1].equals("settle");
        // Warm every caller and callee with empty arrays and no collection;
        // only the 16 MB calls below are judged.
        for (int round = 0; round < (settle ? 2 : 1); round++) {
            for (int i = 0; i < 20_000; i++) {
                sink += DERIVED.superCaller(0, false) ? 1 : 0;
                sink += DERIVED.privateCaller(0, false) ? 1 : 0;
                sink += staticCaller(0, false) ? 1 : 0;
                if (more) {
                    sink += virtualCaller(0, false) ? 1 : 0;
                    sink += interfaceCaller(0, false) ? 1 : 0;
                    sink += splicedCaller(0, false) ? 1 : 0;
                }
            }
            if (settle && round == 0) {
                try {
                    Thread.sleep(2000);
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                }
            }
        }
        guarded("super-special", 0);
        guarded("private-special", 1);
        guarded("static-direct", 2);
        int cases = 3;
        if (more) {
            guarded("virtual-direct", 3);
            guarded("interface-direct", 4);
            guarded("spliced-direct", 5);
            cases = 6;
        }
        if (failures == 0) {
            System.out.println("PASS all " + cases);
        } else {
            System.out.println("FAIL " + failures + " of " + cases);
            System.exit(1);
        }
    }
}
