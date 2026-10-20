// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Item 3 of docs/internal/fixed-bugs/interpreter-L6-a-compiled-frame-in-a-call-at-the-redefinition-keeps-its-old-splice-FIXED-20261007.md
// (2026-10-07): an optimizing-tier body whose frame at a call names a value
// with no machine location -- here javac's for-each `Iterator`, which escape
// analysis scalar-replaces once `ArrayList.iterator()`, `hasNext()` and
// `next()` are spliced -- inside a call when a class it spliced is
// retransformed. The first read of the spliced `Callee.value()` after the
// call returns must run the NEW bytecode (HotSpot returns into the
// interpreter at the call's return point, re-allocating the iterator).
//
// Rows:
//   parked  a worker warms `loop` up (a method-entry body, reached through
//           reflection only), then parks in the first iteration's gate while
//           main retransforms `Callee`;
//   self    `loop` warmed up on main, then the first iteration's gate
//           retransforms on the calling thread itself.
// The gate carries an exception table, so no tier splices it; the iterator
// never reaches it.
//
// HOTSPOT_EXPECTED_BEGIN (Temurin 25.0.3 with the agent, default flags and -Xint)
// parked first-after-new=true sum=3
// self first-after-new=true sum=3
// HOTSPOT_EXPECTED_END
// Without the agent both VMs print `no agent`. Positive control,
// `CRATONVM_DBG_JITC=1`: an `ir-post-call-exit site at bci=N of
// I24ForEachPostCallExit.loop...` line for the gate's successor (before the
// fix: `ir-post-call-exit REFUSED local N not current`).
//
// SETUP: a jar whose manifest has
//     Premain-Class: I24ForEachPostCallExit$Agent
//     Can-Retransform-Classes: true
// containing I24ForEachPostCallExit*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar I24ForEachPostCallExit
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.lang.reflect.Method;
import java.security.ProtectionDomain;
import java.util.ArrayList;
import java.util.concurrent.CountDownLatch;

public class I24ForEachPostCallExit {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class Callee {
        static int value() {
            return 12345;
        }
    }

    /** Rewrites `sipush 12345; ireturn` to answer 12345 + n on the n-th transform. */
    static final class Bump implements ClassFileTransformer {
        int bumps;

        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (!"I24ForEachPostCallExit$Callee".equals(className)) {
                return null;
            }
            bumps++;
            byte[] out = bytes.clone();
            for (int i = 0; i + 3 < out.length; i++) {
                if (out[i] == 0x11 && out[i + 1] == 0x30 && out[i + 2] == 0x39
                        && out[i + 3] == (byte) 0xAC) {
                    out[i + 2] = (byte) (0x39 + bumps);
                }
            }
            return out;
        }
    }

    static final long WARM = 1L << 16;
    static final CountDownLatch OPEN = new CountDownLatch(0);
    static final ArrayList<Integer> LIST = new ArrayList<>();
    static volatile boolean parkedIn;
    static volatile int[] parkedResult;
    static volatile long sink;

    /** A real call (its exception table keeps it out of every splice). */
    static void gate(CountDownLatch latch, boolean now) {
        try {
            if (latch != OPEN) {
                parkedIn = true;
            }
            latch.await();
            if (now) {
                inst.retransformClasses(Callee.class);
            }
        } catch (Exception e) {
            throw new RuntimeException(e);
        }
    }

    /** {the value the first iteration read right after its gate, the elements' sum}. */
    public static int[] loop(CountDownLatch latch, boolean now) {
        int first = 0;
        int sum = 0;
        int k = 0;
        for (Integer x : LIST) {
            gate(k == 0 ? latch : OPEN, now && k == 0);
            int v = Callee.value();
            if (k == 0) {
                first = v;
            }
            sum += x;
            k++;
        }
        return new int[] {first, sum};
    }

    static int[] call(Method m, CountDownLatch latch, boolean now) throws Exception {
        return (int[]) m.invoke(null, latch, now);
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRetransformClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        i.addTransformer(new Bump(), true);
        LIST.add(1);
        LIST.add(1);
        LIST.add(1);
        Method m = I24ForEachPostCallExit.class.getMethod("loop", CountDownLatch.class, boolean.class);

        CountDownLatch go = new CountDownLatch(1);
        Thread worker = new Thread(() -> {
            try {
                long sum = 0;
                for (long k = 0; k < WARM; k++) {
                    sum += call(m, OPEN, false)[1];
                }
                sink = sum;
                int before = Callee.value();
                int[] r = call(m, go, false);
                parkedResult = new int[] {before, r[0], r[1]};
            } catch (Exception e) {
                throw new RuntimeException(e);
            }
        });
        worker.start();
        while (!parkedIn) {
            Thread.sleep(1);
        }
        Thread.sleep(50);
        i.retransformClasses(Callee.class);
        go.countDown();
        worker.join();
        int[] p = parkedResult;
        System.out.println("parked first-after-new=" + (p[1] != p[0]) + " sum=" + p[2]);

        long sum = 0;
        for (long k = 0; k < WARM; k++) {
            sum += call(m, OPEN, false)[1];
        }
        sink = sum;
        int before = Callee.value();
        int[] r = call(m, OPEN, true);
        System.out.println("self first-after-new=" + (r[0] != before) + " sum=" + r[1]);
    }
}
