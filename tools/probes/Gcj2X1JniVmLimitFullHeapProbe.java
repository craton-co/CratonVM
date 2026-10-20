// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;

/**
 * gcj j2/x1 (2026-10-19): a JNI array request past HotSpot's array limit on a
 * FULL heap. HotSpot throws its preallocated
 * {@code Universe::out_of_memory_error_array_size}, so the message is
 * {@code Requested array size exceeds VM limit} however full the heap is.
 * CratonVM builds the error without a collection (the JNI door holds raw
 * locals) and, when even that cannot allocate, fell back to the
 * {@code Java heap space} singleton (gcj j1/jn's {@code raise_jni_oom_message});
 * gcj j2/x1 falls back to the message's preallocated error on Generational.
 *
 * <p>Reuses the native half of {@link Gcj1JnArrayLimitProbe} (its DLL, e.g.
 * {@code gcj1jnal.dll}, built from {@code tools/probes/jni/Gcj1JnArrayLimitProbe.c}).
 *
 * <p>stdout (HotSpot {@code -XX:+UseSerialGC -Xmx64m}, by construction):
 * <pre>
 *   filled=true
 *   byte[MAX]: java.lang.OutOfMemoryError: Requested array size exceeds VM limit
 *   long[MAX-1]: java.lang.OutOfMemoryError: Requested array size exceeds VM limit
 *   PROBE-OK
 * </pre>
 * stderr (informational): {@code [x1-vmlimit] hoard=<n>}.
 *
 * <p>Usage: {@code Gcj2X1JniVmLimitFullHeapProbe <library>}.
 */
public final class Gcj2X1JniVmLimitFullHeapProbe {

    /** The heap's whole content while the requests run. */
    static ArrayList<Object> hoard;

    /** Messages captured with the heap full (no allocation in the catch). */
    static final String[] MESSAGES = new String[2];

    static final String[] LABELS = {"byte[MAX]", "long[MAX-1]"};

    /** Fill the heap until even a 16-byte object is refused. */
    static boolean fill() {
        hoard = new ArrayList<>(1 << 16);
        int size = 1 << 16;
        boolean full = false;
        while (size >= 2) {
            try {
                while (true) {
                    hoard.add(new long[size]);
                }
            } catch (OutOfMemoryError e) {
                size >>= 1;
                full = true;
            }
        }
        // The last few bytes: plain objects until refused (twice, so a
        // refill of a thread-local buffer cannot hide room).
        for (int round = 0; round < 2; round++) {
            try {
                while (true) {
                    hoard.add(new Object());
                }
            } catch (OutOfMemoryError e) {
                full = true;
            }
        }
        return full;
    }

    public static void main(String[] args) {
        System.load(args[0]);
        final int max = Integer.MAX_VALUE;
        // Resolve both natives and the error class before the heap is full.
        try {
            Gcj1JnArrayLimitProbe.newByteArray(max);
        } catch (OutOfMemoryError warm) {
            // expected: the VM limit
        }
        try {
            Gcj1JnArrayLimitProbe.newLongArray(max - 1);
        } catch (OutOfMemoryError warm) {
            // expected: the VM limit
        }
        boolean filled = fill();
        try {
            Object a = Gcj1JnArrayLimitProbe.newByteArray(max);
            MESSAGES[0] = a == null ? "null" : "allocated";
        } catch (OutOfMemoryError e) {
            MESSAGES[0] = e.getMessage();
        }
        try {
            Object a = Gcj1JnArrayLimitProbe.newLongArray(max - 1);
            MESSAGES[1] = a == null ? "null" : "allocated";
        } catch (OutOfMemoryError e) {
            MESSAGES[1] = e.getMessage();
        }
        int held = hoard.size();
        hoard = null;
        System.gc();
        System.err.println("[x1-vmlimit] hoard=" + held);
        System.out.println("filled=" + filled);
        boolean ok = filled;
        for (int i = 0; i < 2; i++) {
            System.out.println(LABELS[i] + ": java.lang.OutOfMemoryError: " + MESSAGES[i]);
            ok &= "Requested array size exceeds VM limit".equals(MESSAGES[i]);
        }
        System.out.println(ok ? "PROBE-OK" : "PROBE-FAIL");
    }
}
