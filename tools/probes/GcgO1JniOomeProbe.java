// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gcg g1/o (2026-10-01): the deciding row of
 * {@code docs/internal/gc/gcf-f1o-jni-oome-message-and-chokepoint-FIXED-20261001.md}.
 * The FIRST {@code OutOfMemoryError} of the run is raised by a JNI allocation
 * ({@code NewByteArray} from C, every array held by a global reference), so
 * both halves of the page are visible on one row:
 * <ul>
 *   <li>the message: HotSpot's JNI allocation is an ordinary heap allocation,
 *       so the error reads exactly {@code Java heap space};</li>
 *   <li>the chokepoint: with {@code -XX:+ExitOnOutOfMemoryError}, HotSpot's
 *       {@code report_java_out_of_memory} prints
 *       {@code Terminating due to java.lang.OutOfMemoryError: Java heap space}
 *       on stdout and exits with status 3 at that JNI allocation -- before
 *       any line below is printed.</li>
 * </ul>
 * HotSpot 25 ({@code java -XX:+UseSerialGC -Xmx64m -cp <dir> GcgO1JniOomeProbe /tmp/libgcgo1jnioome.so}):
 * <pre>
 *   jni-oome-class=java.lang.OutOfMemoryError
 *   jni-oome-message=Java heap space
 *   after-release ok
 * </pre>
 * With {@code -XX:+ExitOnOutOfMemoryError} (exit status 3):
 * <pre>
 *   Terminating due to java.lang.OutOfMemoryError: Java heap space
 * </pre>
 * Diagnostics on stderr. Native half: {@code tools/probes/jni/GcgO1JniOomeProbe.c}.
 */
public final class GcgO1JniOomeProbe {
    static native int fill(int chunk);

    static native int release();

    static long[] big;

    public static void main(String[] args) {
        System.load(args[0]);
        // Warm both streams' write paths and the classes the report uses
        // before the heap is full; prints nothing.
        System.out.print("");
        System.err.print("");
        Throwable thrown = null;
        try {
            fill(64 * 1024);
        } catch (Throwable t) {
            thrown = t;
        }
        final int held = release();
        if (thrown == null) {
            System.out.println("jni-oome=none");
        } else {
            System.out.println("jni-oome-class=" + thrown.getClass().getName());
            System.out.println("jni-oome-message=" + thrown.getMessage());
        }
        System.gc();
        boolean ok;
        try {
            big = new long[1 << 20];
            ok = true;
        } catch (OutOfMemoryError e) {
            ok = false;
        }
        big = null;
        System.out.println(ok ? "after-release ok" : "after-release oome");
        System.err.println("[probe] arrays held at the refusal=" + held);
    }
}
