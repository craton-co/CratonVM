// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.time.Duration;

/**
 * gci i22/rv (finding 3 of gci-i22rv-i21-default-path-review-findings): a
 * virtual thread's `Unsafe.park` with an elapsed or non-positive deadline
 * returns at once, as HotSpot 25's `Parker::park` does. On a VM-scheduled
 * virtual thread (`CRATONVM_VT_CONTINUATIONS=0`) the native turned that
 * deadline into `wake_after_nanos: 0`, the UNTIMED yield, and the thread
 * parked until an `unpark` that never came.
 *
 * `sun.misc.Unsafe.park` is reached by reflection (it is deprecated for
 * removal; no compile-time reference), and delegates to the internal
 * `Unsafe.park(ZJ)V` native. `LockSupport` never passes these arguments for a
 * virtual thread (it converts to relative nanos and skips a non-positive
 * wait), so the probe calls `Unsafe` directly.
 *
 *   GciRv22UnsafeParkVtProbe
 *
 * stdout (deterministic, the same on HotSpot):
 *   rv22 vt abs-zero returned=true
 *   rv22 vt abs-past returned=true
 *   rv22 vt rel-minus-one returned=true
 * stderr: the measured milliseconds.
 */
public class GciRv22UnsafeParkVtProbe {
    public static void main(String[] args) throws Exception {
        Class<?> c = Class.forName("sun.misc.Unsafe");
        Field f = c.getDeclaredField("theUnsafe");
        f.setAccessible(true);
        final Object u = f.get(null);
        final Method park = c.getMethod("park", boolean.class, long.class);
        String[] labels = {"abs-zero", "abs-past", "rel-minus-one"};
        for (int i = 0; i < labels.length; i++) {
            final int k = i;
            final long[] took = new long[] {-1L};
            Thread vt = Thread.ofVirtual().name("rv22-" + labels[i]).start(() -> {
                long s = System.nanoTime();
                try {
                    if (k == 0) {
                        park.invoke(u, true, 0L);
                    } else if (k == 1) {
                        park.invoke(u, true, System.currentTimeMillis() - 5L);
                    } else {
                        park.invoke(u, false, -1L);
                    }
                } catch (ReflectiveOperationException e) {
                    System.err.println("[rv22] " + e);
                }
                took[0] = (System.nanoTime() - s) / 1_000_000L;
            });
            boolean ended = vt.join(Duration.ofSeconds(20));
            System.err.println("[rv22] " + labels[i] + " ms=" + took[0] + " ended=" + ended);
            System.out.println("rv22 vt " + labels[i] + " returned=" + ended);
        }
    }
}
