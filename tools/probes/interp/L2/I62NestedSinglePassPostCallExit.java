// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// The positive control of a post-call exit after a real call inside a NESTED
// single-pass splice
// (docs/internal/fixed-bugs/i62-L2-proposal-post-call-exits-inside-a-nested-single-pass-splice-FIXED-20261007.md,
// `CRATONVM_JIT_SP_NESTED_POST_CALL_SPLICE_EXITS`): the wave-59 probe
// `L2W59SinglePassPostCallExitInsideASplice` one level down. `entry` splices
// `outer`, whose body splices `step` in turn (a NESTED splice), whose real
// call (`block` / `retransform`) parks or retransforms `Callee` while the
// frame is inside it. HotSpot's deopt returns into the interpreter at the
// call's successor in `step`'s own frame, so the read of `Callee.value()`
// right after the call runs the NEW bytecode, and `outer` adds its own read
// after `step` returns.
//
// Rows (`first-after-new`: `step`'s read after the call saw the new value;
// `outer-after-new`: so did `outer`'s read after `step` returned):
//   cold-self  `entrySelf(true)` once, cold (interpreted);
//   parked     a worker warms `entry(OPEN)` up, then `entry(GO)` parks in
//              `GO.await()` while main retransforms Callee;
//   self       `entrySelf(false)` warmed up, then `entrySelf(true)`;
//   parked-osr a loop (an OSR-tier compile) splicing `outerOsr` (which
//              splices `stepOsr`) parks in its last iteration while main
//              retransforms Callee;
//   self-osr   a loop splicing `outerSelf` retransforms in its last
//              iteration on its own thread.
//
// HOTSPOT_EXPECTED_BEGIN (Temurin 25.0.3 with the agent, default flags and -Xint)
// cold-self first-after-new=true outer-after-new=true
// parked first-after-new=true outer-after-new=true
// self first-after-new=true outer-after-new=true
// parked-osr first-after-new=true outer-after-new=true
// self-osr first-after-new=true outer-after-new=true
// HOTSPOT_EXPECTED_END
// Without the agent both VMs print `no agent`.
//
// CratonVM: the single-pass control is `CRATONVM_TIER_C2_THRESHOLD=1000000000
// CRATONVM_JIT_OSR_OPTIMIZING=0`. With
// `CRATONVM_JIT_SP_NESTED_POST_CALL_SPLICE_EXITS=0` the `parked` and `self`
// rows print `first-after-new=false` (the activation runs the rest of both
// splices on the old `Callee.value()`); with it on (default), HotSpot's
// lines. POSITIVE CONTROL (`CRATONVM_DBG_JITC=1`):
//   `[cratonvm-jitc] sp-splice-post-call-exit site nop=.. callee=...step:(...)I bci=4 in ...entry...`
// and (`CRATONVM_DBG_DEOPT=1`) a 3-frame chain resume
//   `...entry@.. -> ...outer@.. -> ...step@4`.
//
// SETUP: a jar whose manifest has
//     Premain-Class: I62NestedSinglePassPostCallExit$Agent
//     Can-Retransform-Classes: true
// containing I62NestedSinglePassPostCallExit*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar I62NestedSinglePassPostCallExit
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.lang.reflect.Method;
import java.security.ProtectionDomain;
import java.util.concurrent.CountDownLatch;

public class I62NestedSinglePassPostCallExit {
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

    /**
     * Rewrites `sipush 12345; ireturn` (11 30 39 AC) to answer 12345 + n on
     * the n-th transform (a retransform starts again from the original
     * bytes).
     */
    static final class Bump implements ClassFileTransformer {
        int bumps;

        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (!"I62NestedSinglePassPostCallExit$Callee".equals(className)) {
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
    static volatile boolean parkedIn;
    static final CountDownLatch GO = new CountDownLatch(1);
    static final CountDownLatch OPEN = new CountDownLatch(0);
    static volatile int[] parkedResult;
    static volatile long sink;

    /** A real call: its exception table keeps it out of every splice. */
    static void block(CountDownLatch latch) {
        try {
            if (latch == GO) {
                parkedIn = true;
            }
            latch.await();
        } catch (InterruptedException e) {
            throw new RuntimeException(e);
        }
    }

    /** A real call, for the same reason. */
    static void retransform(boolean now) {
        try {
            if (now) {
                inst.retransformClasses(Callee.class);
            }
        } catch (Exception e) {
            throw new RuntimeException(e);
        }
    }

    /** Spliced into `entry`: a real call, then the splice of Callee. No arrays. */
    static int step(CountDownLatch latch) {
        block(latch);
        return Callee.value();
    }

    /** Spliced into `entrySelf`: a real call, then the splice of Callee. */
    static int stepSelf(boolean now) {
        retransform(now);
        return Callee.value();
    }

    /** Spliced into `entry`; splices `step` in turn (a NESTED splice). */
    static int outer(CountDownLatch latch) {
        int stepped = step(latch);
        return stepped * 100000 + Callee.value();
    }

    /** Spliced into `entrySelf`; splices `stepSelf` in turn. */
    static int outerSelf(boolean now) {
        int stepped = stepSelf(now);
        return stepped * 100000 + Callee.value();
    }

    /** Reached through reflection only: a method-entry body of its own. */
    public static int entry(CountDownLatch latch) {
        return outer(latch);
    }

    /** Reached through reflection only, as `entry`. */
    public static int entrySelf(boolean now) {
        return outerSelf(now);
    }

    static final long SPIN = 1L << 18;
    static final CountDownLatch GO_OSR = new CountDownLatch(1);
    static volatile boolean parkedOsrIn;
    static volatile int[] parkedOsrResult;

    /** A real call for the OSR rows: as `block`, on its own latch flag. */
    static void blockOsr(CountDownLatch latch) {
        try {
            if (latch == GO_OSR) {
                parkedOsrIn = true;
            }
            latch.await();
        } catch (InterruptedException e) {
            throw new RuntimeException(e);
        }
    }

    /** Spliced into `outerOsr`, which `loop` splices: a NESTED splice. */
    static int stepOsr(CountDownLatch latch) {
        blockOsr(latch);
        return Callee.value();
    }

    /** Spliced into `loop`; splices `stepOsr` in turn. */
    static int outerOsr(CountDownLatch latch) {
        int stepped = stepOsr(latch);
        return stepped * 100000 + Callee.value();
    }

    /** Spins `n` iterations (an OSR-tier compile); iteration `k` parks. */
    static int[] loop(long n, long k) {
        int before = 0;
        int at = 0;
        long sum = 0;
        for (long j = 0; j < n; j++) {
            if (j == k) {
                before = Callee.value();
                at = outerOsr(GO_OSR);
            } else {
                sum += outerOsr(OPEN);
            }
        }
        sink = sum;
        return new int[] {before, at};
    }

    /** As `loop`, but iteration `k` retransforms on this thread. */
    static int[] loopSelf(long n, long k) {
        int before = 0;
        int at = 0;
        long sum = 0;
        for (long j = 0; j < n; j++) {
            if (j == k) {
                before = Callee.value();
                at = outerSelf(true);
            } else {
                sum += outerSelf(false);
            }
        }
        sink = sum;
        return new int[] {before, at};
    }

    /** `first-after-new=.. outer-after-new=..` for a result of `outer`. */
    static String rows(int before, int result) {
        int stepped = result / 100000;
        int outerRead = result % 100000;
        return "first-after-new=" + (stepped != before) + " outer-after-new=" + (outerRead != before);
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRetransformClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        i.addTransformer(new Bump(), true);
        Class<?> me = I62NestedSinglePassPostCallExit.class;
        Method entry = me.getMethod("entry", CountDownLatch.class);
        Method entrySelf = me.getMethod("entrySelf", boolean.class);

        // The fixed case first, cold.
        int coldBefore = Callee.value();
        int coldAfter = (Integer) entrySelf.invoke(null, true);
        System.out.println("cold-self " + rows(coldBefore, coldAfter));

        Thread worker = new Thread(() -> {
            try {
                long sum = 0;
                for (long k = 0; k < WARM; k++) {
                    sum += (Integer) entry.invoke(null, OPEN);
                }
                sink = sum;
                int before = Callee.value();
                parkedResult = new int[] {before, (Integer) entry.invoke(null, GO)};
            } catch (Exception e) {
                throw new RuntimeException(e);
            }
        });
        worker.start();
        while (!parkedIn) {
            Thread.sleep(1);
        }
        // Parked in `GO.await()`, a call made from inside `step`'s splice.
        Thread.sleep(50);
        i.retransformClasses(Callee.class);
        GO.countDown();
        worker.join();
        int[] p = parkedResult;
        System.out.println("parked " + rows(p[0], p[1]));

        long sum = 0;
        for (long k = 0; k < WARM; k++) {
            sum += (Integer) entrySelf.invoke(null, false);
        }
        sink = sum;
        int before = Callee.value();
        int after = (Integer) entrySelf.invoke(null, true);
        System.out.println("self " + rows(before, after));

        Thread osr = new Thread(() -> parkedOsrResult = loop(SPIN + 1, SPIN));
        osr.start();
        while (!parkedOsrIn) {
            Thread.sleep(1);
        }
        Thread.sleep(50);
        i.retransformClasses(Callee.class);
        GO_OSR.countDown();
        osr.join();
        int[] q = parkedOsrResult;
        System.out.println("parked-osr " + rows(q[0], q[1]));
        int[] r = loopSelf(SPIN + 1, SPIN);
        System.out.println("self-osr " + rows(r[0], r[1]));
    }
}
