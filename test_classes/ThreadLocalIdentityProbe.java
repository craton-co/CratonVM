// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * Does a `ThreadLocal.get()` return the SAME instance on repeated calls?
 *
 * This is the mechanism `sun.nio.ch.Util.getTemporaryDirectBuffer` relies on
 * to reuse one direct buffer per thread across every
 * `FileChannel.read(HeapByteBuffer)`. If `get()` ever re-runs
 * `initialValue()`, that cache misses on every read and each read allocates
 * a fresh `DirectByteBuffer` plus a Cleaner registration — which is the
 * shape a stack sample of CratonVM loading a GGUF model kept catching.
 *
 * `Util`'s cache is a `TerminatingThreadLocal` (a `jdk.internal.misc`
 * subclass of `ThreadLocal` that also gets a callback on thread death), so
 * both the plain and the subclassed forms are probed: a subclass that
 * overrides nothing relevant must behave identically, and if it does not,
 * that is the defect.
 */
public class ThreadLocalIdentityProbe {

    static final class Box {
        static int constructed = 0;
        final int serial;
        Box() {
            serial = ++constructed;
        }
    }

    static final ThreadLocal<Box> PLAIN = new ThreadLocal<>() {
        @Override
        protected Box initialValue() {
            return new Box();
        }
    };

    /** Stands in for `TerminatingThreadLocal`: a subclass with extra state. */
    static class SubTL<T> extends ThreadLocal<T> {
        final java.util.function.Supplier<T> mk;
        SubTL(java.util.function.Supplier<T> mk) {
            this.mk = mk;
        }
        @Override
        protected T initialValue() {
            return mk.get();
        }
    }

    static final SubTL<Box> SUBCLASSED = new SubTL<>(Box::new);

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 100000;

        Box first = PLAIN.get();
        int distinct = 0;
        for (int i = 0; i < iters; i++) {
            if (PLAIN.get() != first) {
                distinct++;
            }
        }
        Box subFirst = SUBCLASSED.get();
        int subDistinct = 0;
        for (int i = 0; i < iters; i++) {
            if (SUBCLASSED.get() != subFirst) {
                subDistinct++;
            }
        }

        System.out.println("TLIDENTITY iters=" + iters
                + " plain_not_same=" + distinct
                + " subclassed_not_same=" + subDistinct
                + " boxes_constructed=" + Box.constructed);
        System.out.println((distinct == 0 && subDistinct == 0 && Box.constructed == 2)
                ? "TLIDENTITY verdict=STABLE"
                : "TLIDENTITY verdict=RE_INITIALISES");
    }
}
