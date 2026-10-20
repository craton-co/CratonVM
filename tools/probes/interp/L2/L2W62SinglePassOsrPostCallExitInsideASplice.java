// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 62, lane L2 -- the OSR-tier twin of
// `L2W59SinglePassPostCallExitInsideASplice`: a SINGLE-PASS OSR body (a hot
// loop) that SPLICED a small callee `step` is inside a REAL call made from
// within that splice when the class of a body the splice copied
// (`Callee.value()`) is retransformed. HotSpot's deopt returns into the
// interpreter at the call's successor in `step`'s own frame, so the read of
// `Callee.value()` right after the call runs the NEW bytecode.
//
// Rows (the cold one first, then again warm):
//   cold-self    `loopSelf(1, 0)` once, cold (interpreted);
//   parked-osr   a worker spins `loop(n, k)`, whose iteration `k` calls
//                `step(GO)`, which parks in `GO.await()` (called from `block`,
//                a real call: its exception table keeps it out of every
//                splice) while main retransforms Callee;
//   self-osr     `loopSelf(n, k)` on main, whose iteration `k` calls
//                `stepSelf(true)`, whose `retransform` call retransforms
//                Callee on the calling thread itself.
// The loops live in their own static methods and are entered once each, so
// they run as OSR bodies (no method-entry compile reaches them first).
//
// HOTSPOT_EXPECTED_BEGIN (Temurin 25.0.3 with the agent, default flags)
// cold-self first-after-new=true
// parked-osr first-after-new=true
// self-osr first-after-new=true
// HOTSPOT_EXPECTED_END
// Without the agent both VMs print `no agent`.
//
// CratonVM (lane's Windows debug build, measured; "Progress (wave 62)" of
// docs/internal/fixed-bugs/i41-L1-compiled-bodies-without-an-exit-finish-their-activation-compiled-FIXED-20261007.md):
// * the SINGLE-PASS OSR control is
//   `CRATONVM_TIER_C2_THRESHOLD=1000000000 CRATONVM_JIT_OSR_OPTIMIZING=0`.
//   Base 33f99173a7 and `CRATONVM_JIT_SP_OSR_POST_CALL_SPLICE_EXITS=0`:
//   `parked-osr` and `self-osr` print `first-after-new=false` (the OSR-tier
//   compile put no post-call site after the real call inside `step`'s
//   splice, so the activation ran the rest of the splice on the old nested
//   `Callee.value()`); switch on: HotSpot's lines, also `--compatible`.
// * default flags (optimizing OSR bodies), `CRATONVM_TIER_C2_THRESHOLD=1000000000`
//   alone, `--nojit` and `--compatible --nojit`: HotSpot's lines, switch on
//   or off.
// POSITIVE CONTROL (single-pass OSR control, switch on):
// `CRATONVM_DBG_JITC=1` prints
//   `[cratonvm-jitc] sp-splice-post-call-exit site nop=.. callee=...step:(Ljava/util/concurrent/CountDownLatch;)I bci=4 in ...loop:(JJ)[J`
// (and `stepSelf` in `loopSelf`), and `CRATONVM_DBG_DEOPT=1`
//   `OSR-exit CHAIN transfer: L2W62SinglePassOsrPostCallExitInsideASplice.loop in place @38, then 1 pushed frame(s)`
// (and `loopSelf ... @36`). Switch off: `sp-splice-post-call-exit-census
// osr-tier-splice` lines instead, and no site.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L2W62SinglePassOsrPostCallExitInsideASplice$Agent
//     Can-Retransform-Classes: true
// containing L2W62SinglePassOsrPostCallExitInsideASplice*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L2W62SinglePassOsrPostCallExitInsideASplice
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;
import java.util.concurrent.CountDownLatch;

public class L2W62SinglePassOsrPostCallExitInsideASplice {
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
            if (!"L2W62SinglePassOsrPostCallExitInsideASplice$Callee".equals(className)) {
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

    static final long SPIN = 1L << 18;
    static volatile boolean parkedIn;
    static final CountDownLatch GO = new CountDownLatch(1);
    static final CountDownLatch OPEN = new CountDownLatch(0);
    static volatile long[] parkedResult;
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

    /** Spliced into `loop`: a real call, then the splice of Callee. No arrays. */
    static int step(CountDownLatch latch) {
        block(latch);
        return Callee.value();
    }

    /** Spliced into `loopSelf`: a real call, then the splice of Callee. */
    static int stepSelf(boolean now) {
        retransform(now);
        return Callee.value();
    }

    /**
     * Spins `n` iterations; iteration `k` parks in `step(GO)`. Answers
     * `{the value before, the value iteration k read}`.
     */
    static long[] loop(long n, long k) {
        long before = 0;
        long at = 0;
        long sum = 0;
        for (long j = 0; j < n; j++) {
            if (j == k) {
                before = Callee.value();
                at = step(GO);
            } else {
                sum += step(OPEN);
            }
        }
        sink = sum;
        return new long[] {before, at};
    }

    /** As `loop`, but iteration `k` retransforms on this thread. */
    static long[] loopSelf(long n, long k) {
        long before = 0;
        long at = 0;
        long sum = 0;
        for (long j = 0; j < n; j++) {
            if (j == k) {
                before = Callee.value();
                at = stepSelf(true);
            } else {
                sum += stepSelf(false);
            }
        }
        sink = sum;
        return new long[] {before, at};
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRetransformClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        i.addTransformer(new Bump(), true);

        // The fixed case first, cold: one iteration, which retransforms.
        long[] cold = loopSelf(1, 0);
        System.out.println("cold-self first-after-new=" + (cold[1] != cold[0]));

        Thread worker = new Thread(() -> parkedResult = loop(SPIN + 1, SPIN));
        worker.start();
        while (!parkedIn) {
            Thread.sleep(1);
        }
        // Parked in `GO.await()`, a call made from inside `step`'s splice.
        Thread.sleep(50);
        i.retransformClasses(Callee.class);
        GO.countDown();
        worker.join();
        long[] p = parkedResult;
        System.out.println("parked-osr first-after-new=" + (p[1] != p[0]));

        long[] s = loopSelf(SPIN + 1, SPIN);
        System.out.println("self-osr first-after-new=" + (s[1] != s[0]));
    }
}
