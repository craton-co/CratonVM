// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;

/**
 * gcj j1/jn (2026-10-19): a JNI array request past HotSpot's array limit
 * ({@code Integer.MAX_VALUE - 2}, every element type) throws
 * {@code OutOfMemoryError: Requested array size exceeds VM limit} without a
 * collection
 * ({@code docs/known-issues/gc/gcj-j1jn-jni-array-past-the-vm-limit-is-java-heap-space-20261019.md}).
 * Native half: {@code tools/probes/jni/Gcj1JnArrayLimitProbe.c} (build lines
 * there).
 *
 * <p>HotSpot ({@code java -XX:+UseSerialGC -Xmx64m}) prints:
 *
 * <pre>
 *   byte[16]: allocated
 *   byte[MAX]: java.lang.OutOfMemoryError: Requested array size exceeds VM limit gcs=0
 *   byte[MAX-1]: java.lang.OutOfMemoryError: Requested array size exceeds VM limit gcs=0
 *   long[MAX-1]: java.lang.OutOfMemoryError: Requested array size exceeds VM limit gcs=0
 *   Object[MAX-1]: java.lang.OutOfMemoryError: Requested array size exceeds VM limit gcs=0
 *   byte[MAX-2]: java.lang.OutOfMemoryError: Java heap space
 * </pre>
 *
 * {@code gcs} is the change in the collectors' summed collection counts across
 * the one JNI call. The last line is the control: a length inside the limit is
 * a heap refusal (and collects; its count is not printed).
 *
 * <p>Usage: {@code Gcj1JnArrayLimitProbe <library>}.
 */
public final class Gcj1JnArrayLimitProbe {

    static native Object newByteArray(int len);

    static native Object newLongArray(int len);

    static native Object newObjectArray(int len, Class<?> component);

    interface Request {
        Object run();
    }

    static long gcCount() {
        long n = 0;
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            n += Math.max(0, b.getCollectionCount());
        }
        return n;
    }

    static void attempt(String label, boolean countGcs, Request r) {
        long before = gcCount();
        String line;
        try {
            Object a = r.run();
            line = label + ": " + (a != null ? "allocated" : "null without an exception");
        } catch (OutOfMemoryError e) {
            line = label + ": " + e.getClass().getName() + ": " + e.getMessage();
            if (countGcs) {
                line += " gcs=" + (gcCount() - before == 0 ? "0" : ">0");
            }
        }
        System.out.println(line);
    }

    public static void main(String[] args) {
        System.load(args[0]);
        final int max = Integer.MAX_VALUE;
        gcCount(); // the beans exist before the first counted call
        attempt("byte[16]", false, () -> newByteArray(16));
        attempt("byte[MAX]", true, () -> newByteArray(max));
        attempt("byte[MAX-1]", true, () -> newByteArray(max - 1));
        attempt("long[MAX-1]", true, () -> newLongArray(max - 1));
        attempt("Object[MAX-1]", true, () -> newObjectArray(max - 1, Object.class));
        attempt("byte[MAX-2]", false, () -> newByteArray(max - 2));
    }
}
