// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Item 1 of docs/internal/fixed-bugs/interpreter-L6-a-compiled-frame-in-a-call-at-the-redefinition-keeps-its-old-splice-FIXED-20261007.md
// (2026-10-07): a `static synchronized` method -- a loop-free single-pass
// METHOD-ENTRY body -- inside a call when a class it spliced is
// retransformed. The first read of the spliced `Callee.value()` after the
// call returns must run the NEW bytecode (HotSpot returns into the
// interpreter at the call's return point).
//
// Rows:
//   parked  a worker warms `syncGate` up, then parks in its gate while main
//           retransforms `Callee`;
//   self    `syncGate` warmed up on main, then its gate retransforms on the
//           calling thread itself;
//   parked-plain / self-plain  the same through a NON-synchronized twin.
// The gate carries an exception table, so the optimizing tier keeps it a real
// call; under `CRATONVM_JIT=tier-c2-threshold=1000000000` the bodies stay
// single-pass, where a call the compile bound directly keeps no dispatch
// record (`op_invoke.rs::keeps_a_call_record`).
//
// HOTSPOT_EXPECTED_BEGIN (Temurin 25.0.3 with the agent, default flags and -Xint)
// parked first-after-new=true
// self first-after-new=true
// parked-plain first-after-new=true
// self-plain first-after-new=true
// HOTSPOT_EXPECTED_END
// Without the agent both VMs print `no agent`.
//
// SETUP: a jar whose manifest has
//     Premain-Class: I24SyncEntryPostCallExit$Agent
//     Can-Retransform-Classes: true
// containing I24SyncEntryPostCallExit*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar I24SyncEntryPostCallExit
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.lang.reflect.Method;
import java.security.ProtectionDomain;
import java.util.concurrent.CountDownLatch;

public class I24SyncEntryPostCallExit {
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
            if (!"I24SyncEntryPostCallExit$Callee".equals(className)) {
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
    static volatile boolean parkedIn;
    static volatile int[] parkedResult;
    static volatile long sink;

    /** A real call: its exception table keeps it out of every splice. */
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

    public static synchronized int syncGate(CountDownLatch latch, boolean now) {
        gate(latch, now);
        return Callee.value();
    }

    public static int plainGate(CountDownLatch latch, boolean now) {
        gate(latch, now);
        return Callee.value();
    }

    static int call(Method m, CountDownLatch latch, boolean now) throws Exception {
        return (Integer) m.invoke(null, latch, now);
    }

    static void rows(String suffix, Method m) throws Exception {
        CountDownLatch go = new CountDownLatch(1);
        parkedIn = false;
        Thread worker = new Thread(() -> {
            try {
                long sum = 0;
                for (long k = 0; k < WARM; k++) {
                    sum += call(m, OPEN, false);
                }
                sink = sum;
                int before = Callee.value();
                parkedResult = new int[] {before, call(m, go, false)};
            } catch (Exception e) {
                throw new RuntimeException(e);
            }
        });
        worker.start();
        while (!parkedIn) {
            Thread.sleep(1);
        }
        Thread.sleep(50);
        inst.retransformClasses(Callee.class);
        go.countDown();
        worker.join();
        int[] p = parkedResult;
        System.out.println("parked" + suffix + " first-after-new=" + (p[1] != p[0]));

        long sum = 0;
        for (long k = 0; k < WARM; k++) {
            sum += call(m, OPEN, false);
        }
        sink = sum;
        int before = Callee.value();
        int after = call(m, OPEN, true);
        System.out.println("self" + suffix + " first-after-new=" + (after != before));
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRetransformClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        i.addTransformer(new Bump(), true);
        Class<?>[] sig = {CountDownLatch.class, boolean.class};
        rows("", I24SyncEntryPostCallExit.class.getMethod("syncGate", sig));
        rows("-plain", I24SyncEntryPostCallExit.class.getMethod("plainGate", sig));
    }
}
