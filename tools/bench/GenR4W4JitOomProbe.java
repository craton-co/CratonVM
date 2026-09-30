// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gen r4w4/oom (2026-09-24): an {@code OutOfMemoryError} raised by COMPILED
 * array allocation must read exactly as the interpreter's and HotSpot's. Items
 * 1-3 of {@code docs/internal/gc-common-round-20260923/common-w5c-jit-allocation-helpers-bypass-the-shared-oome-rules-FIXED-20260923.md},
 * landed in {@code vm/src/jit/helpers.rs}:
 * <ul>
 *   <li>a length past {@code Integer.MAX_VALUE - 2} is refused with
 *       {@code "Requested array size exceeds VM limit"} and no collection
 *       ({@code jit_newarray}, {@code jit_anewarray_object});</li>
 *   <li>a heap-exhaustion message carries no VM site suffix
 *       ({@code "Java heap space"}, not {@code "Java heap space (alloc_array
 *       length 268435456)"});</li>
 *   <li>the error object is built without a second collection ladder
 *       (visible only in {@code CRATONVM_DBG=gc-stats}: no {@code exceptions}
 *       forced site).</li>
 * </ul>
 * The two allocating methods are warmed first so a JIT build compiles them;
 * under {@code --nojit} the same lines come from the interpreter and must be
 * identical.
 *
 * <p>HotSpot ({@code -XX:+UseSerialGC -Xmx64m}) prints:
 * <pre>
 *   limit-long: OutOfMemoryError "Requested array size exceeds VM limit"
 *   limit-ref: OutOfMemoryError "Requested array size exceeds VM limit"
 *   heap-long: OutOfMemoryError "Java heap space"
 *   heap-ref: OutOfMemoryError "Java heap space"
 *   PASS
 * </pre>
 * Commands:
 * <pre>
 *   java -XX:+UseSerialGC -Xmx64m -cp tools/bench GenR4W4JitOomProbe
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx64m -cp tools/bench GenR4W4JitOomProbe
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx64m --nojit -cp tools/bench GenR4W4JitOomProbe
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx64m -XX:+HeapDumpOnOutOfMemoryError \
 *       -XX:HeapDumpPath=/tmp/gen-jit-oom.hprof -cp tools/bench GenR4W4JitOomProbe
 * </pre>
 * The last must first print, once, {@code java.lang.OutOfMemoryError:
 * Requested array size exceeds VM limit}, {@code Dumping heap to
 * /tmp/gen-jit-oom.hprof ...} and {@code Heap dump file created [...]}:
 * HotSpot's {@code report_java_out_of_memory} dumps for the VM-limit refusal
 * too (delete the file between runs; HotSpot refuses to overwrite one).
 */
public final class GenR4W4JitOomProbe {
    static boolean ok = true;

    static long[] allocLongs(int n) {
        return new long[n];
    }

    static Object[] allocRefs(int n) {
        return new Object[n];
    }

    static void expect(String what, int n, boolean refs, String message) {
        try {
            Object a = refs ? allocRefs(n) : allocLongs(n);
            System.out.println(what + ": no error (" + (a != null) + ")");
            ok = false;
        } catch (OutOfMemoryError e) {
            System.out.println(what + ": OutOfMemoryError \"" + e.getMessage() + "\"");
            ok &= message.equals(e.getMessage());
        }
    }

    public static void main(String[] args) {
        long sum = 0;
        for (int i = 0; i < 200_000; i++) {
            sum += allocLongs(8).length + allocRefs(8).length;
        }
        if (sum != 3_200_000L) {
            System.out.println("warm-up FAILED sum=" + sum);
            ok = false;
        }
        expect("limit-long", Integer.MAX_VALUE, false, "Requested array size exceeds VM limit");
        expect("limit-ref", Integer.MAX_VALUE, true, "Requested array size exceeds VM limit");
        expect("heap-long", 1 << 28, false, "Java heap space");
        expect("heap-ref", 1 << 28, true, "Java heap space");
        System.out.println(ok ? "PASS" : "FAIL");
        if (!ok) {
            System.exit(1);
        }
    }
}
