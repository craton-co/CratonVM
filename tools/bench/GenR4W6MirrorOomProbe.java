// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gen r4w6/review6 (2026-09-24): the first {@code ldc Foo.class} of a class
 * whose {@code java.lang.Class} mirror does not exist yet, run on a heap full of
 * LIVE data, must end in a catchable {@code OutOfMemoryError} (or succeed),
 * never in a VM abort. See
 * {@code docs/internal/gaps/gengc-r4w6-review6-class-mirror-creation-aborts-on-a-full-heap-outside-ldc-FIXED-20260927.md}.
 *
 * <p>Two shapes, after the heap is filled to the last few bytes (big arrays
 * until the first error, then small nodes until the second):
 * <ul>
 *   <li>{@code interp}: the {@code ldc} runs in interpreted {@code main}
 *       ({@code execute_ldc}, made fallible by {@code fd1c23e51});</li>
 *   <li>{@code jit}: the {@code ldc} sits on the cold arm of a method called
 *       50,000 times first, so a JIT build may run it compiled
 *       ({@code jit_ldc_class_cp}, still infallible when this was written).</li>
 * </ul>
 * Whether the {@code ldc} itself throws depends on the VM (HotSpot's CDS
 * archive may already hold the mirror), so each shape prints the same line
 * either way; the outcome goes to stderr as a {@code diag:} line. What is
 * checked is that the process survives, recovers once the data is dropped,
 * and prints {@code PASS}. A {@code FATAL:} line, an abort or a missing line is
 * a failure.
 *
 * <p>HotSpot ({@code -XX:+UseSerialGC -Xmx64m}) prints on stdout:
 * <pre>
 *   interp: ldc-class survived
 *   jit: ldc-class survived
 *   recovered java.util.zip.Adler32 java.util.concurrent.atomic.DoubleAccumulator
 *   PASS
 * </pre>
 * Commands:
 * <pre>
 *   java -XX:+UseSerialGC -Xmx64m -cp tools/bench GenR4W6MirrorOomProbe
 *   timeout 300 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx64m -cp tools/bench GenR4W6MirrorOomProbe
 *   timeout 300 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx64m --nojit -cp tools/bench GenR4W6MirrorOomProbe
 * </pre>
 */
public final class GenR4W6MirrorOomProbe {
    static final class Node {
        final Node next;
        final long v;

        Node(Node next, long v) {
            this.next = next;
            this.v = v;
        }
    }

    static Object[] blocks;
    static Node nodes;
    static volatile Class<?> sink;

    /** Fill the heap with live data: big blocks, then small nodes. */
    static void fillHeap() {
        try {
            while (true) {
                final Object[] b = new Object[1024];
                b[0] = blocks;
                blocks = b;
            }
        } catch (OutOfMemoryError e) {
            // The heap is full to within one block; top it up below.
        }
        try {
            long v = 0;
            while (true) {
                nodes = new Node(nodes, v++);
            }
        } catch (OutOfMemoryError e) {
            // Full to within one node.
        }
    }

    /** The {@code ldc} whose mirror is created here, interpreted. */
    static boolean ldcInterpreted() {
        try {
            sink = java.util.zip.Adler32.class;
            return true;
        } catch (OutOfMemoryError e) {
            return false;
        }
    }

    /** Hot on its {@code Object} arm; the other arm's {@code ldc} runs once. */
    static Class<?> pick(boolean victim) {
        return victim ? java.util.concurrent.atomic.DoubleAccumulator.class : Object.class;
    }

    static boolean ldcCompiled() {
        try {
            sink = pick(true);
            return true;
        } catch (OutOfMemoryError e) {
            return false;
        }
    }

    public static void main(String[] args) {
        for (int i = 0; i < 50_000; i++) {
            sink = pick(false);
        }
        fillHeap();
        final boolean interpOk = ldcInterpreted();
        final boolean jitOk = ldcCompiled();
        blocks = null;
        nodes = null;
        System.out.println("interp: ldc-class survived");
        System.out.println("jit: ldc-class survived");
        System.err.println("diag: interp-ldc=" + (interpOk ? "ok" : "OutOfMemoryError")
                + " jit-ldc=" + (jitOk ? "ok" : "OutOfMemoryError"));
        System.out.println("recovered " + java.util.zip.Adler32.class.getName() + " "
                + java.util.concurrent.atomic.DoubleAccumulator.class.getName());
        System.out.println("PASS");
    }
}
