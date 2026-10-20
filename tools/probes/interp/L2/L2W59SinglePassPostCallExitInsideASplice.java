// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 59, lane L2 -- the positive control of the
// SINGLE-PASS stage of
// docs/internal/fixed-bugs/i39-L3-proposal-post-call-exits-inside-a-splice-FIXED-20261007.md
// (`CRATONVM_JIT_SP_POST_CALL_SPLICE_EXITS`): a single-pass METHOD-ENTRY body
// that SPLICED a small callee `step` is inside a REAL call made from within
// that splice when the class of a body the splice copied (`Callee.value()`)
// is retransformed. HotSpot's deopt returns into the interpreter at the
// call's successor in `step`'s own frame, so the read of `Callee.value()`
// right after the call runs the NEW bytecode.
//
// The twin of `L2W54EntryPostCallExitInsideASplice` (the optimizing tier's
// stage), with an ARRAY-FREE `step`: the single-pass splice walk has no array
// lowering (`L2W54`'s `LATCHES[idx]` keeps its `step` out of every
// single-pass splice -- measured on the base: `entry` is compiled without the
// splice and its own `step` body leaves through an ordinary post-call site).
// Here `step` is `aload_0; invokestatic block; invokestatic Callee.value;
// ireturn`, which the single-pass planner splices (`block` direct-bound,
// `Callee.value` nested).
//
// `entry` / `entrySelf` are called through reflection only, so no compiled
// caller splices them. Rows (the cold one first, then again warm):
//   cold-self  `entrySelf(true)` once, cold (interpreted);
//   parked     a worker warms `entry(OPEN)` up, then `entry(GO)` parks in
//              `GO.await()` (called from `block`, a real call: its
//              exception table keeps it out of every splice) while main
//              retransforms Callee;
//   self       `entrySelf(false)` warmed up, then `entrySelf(true)`, whose
//              `retransform` call (a real call, same reason) retransforms
//              Callee on the calling thread itself.
//
// HOTSPOT_EXPECTED_BEGIN (Temurin 25.0.3 with the agent, default flags)
// cold-self first-after-new=true
// parked first-after-new=true
// self first-after-new=true
// HOTSPOT_EXPECTED_END
// Without the agent both VMs print `no agent`.
//
// CratonVM (lane's debug build, measured):
// * Default flags: `entry` / `entrySelf` usually run their OPTIMIZING body by
//   the time the rows run, which leaves through the wave-54 chain site:
//   HotSpot's lines on the base too.
// * The SINGLE-PASS control is `CRATONVM_TIER_C2_THRESHOLD=1000000000` (no
//   optimizing tier-up; `CRATONVM_C2_SUPERSEDE=0` still compiles optimizing
//   bodies here). With `CRATONVM_JIT_SP_POST_CALL_SPLICE_EXITS=0` the
//   `parked` and `self` rows print `first-after-new=false` (a single-pass
//   splice got no post-call site: `post_call_exit_site_may_be_emitted`
//   refuses inside a splice, so the activation ran the rest of the splice on
//   the old nested `Callee.value()`); with it on, HotSpot's lines.
// * `CRATONVM_C2_SUPERSEDE=0` on the base 40b8e4e9bf: the worker dies with
//   `InternalError: precise deoptimization unavailable for ...block(...)`
//   (the call-site service could not find the withdrawn optimizing `block`
//   body whose chain exit fired) and main throws an NPE;
//   docs/internal/fixed-bugs/interpreter-L2-a-withdrawn-callees-chain-exit-is-refused-at-the-call-site-FIXED-20261023.md
//   (`CRATONVM_DEOPT_CHAIN_POINT_OWNER_BY_RANGE`). HotSpot's lines with it.
// * `--nojit`, `--compatible`: HotSpot's lines.
//
// POSITIVE CONTROL (single-pass control env): `CRATONVM_DBG_JITC=1` prints,
// for the single-pass compile of `entry` / `entrySelf`,
//   `[cratonvm-jitc] sp-splice-post-call-exit site nop=.. callee=...step:(...)I bci=4 in ...entry...`
//   `[cratonvm-jitc] inline-splice ...step(...)I at pc=.. published 1 point(s): admitted, ...`
// and `CRATONVM_DBG_DEOPT=1`
//   `post-call exit verdict #k: ...entry:(...)I id=.. named=true withdrawn=true verdict=0x7`
//   `PRECISE resume of a 2-frame inlined chain: ...entry@4 -> ...step@4`
// (the same for `entrySelf` / `stepSelf`). With the switch off no
// `sp-splice-post-call-exit site` line appears.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L2W59SinglePassPostCallExitInsideASplice$Agent
//     Can-Retransform-Classes: true
// containing L2W59SinglePassPostCallExitInsideASplice*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L2W59SinglePassPostCallExitInsideASplice
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.lang.reflect.Method;
import java.security.ProtectionDomain;
import java.util.concurrent.CountDownLatch;

public class L2W59SinglePassPostCallExitInsideASplice {
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
            if (!"L2W59SinglePassPostCallExitInsideASplice$Callee".equals(className)) {
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

    /** Reached through reflection only: a method-entry body of its own. */
    public static int entry(CountDownLatch latch) {
        return step(latch);
    }

    /** Reached through reflection only, as `entry`. */
    public static int entrySelf(boolean now) {
        return stepSelf(now);
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRetransformClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        i.addTransformer(new Bump(), true);
        Class<?> me = L2W59SinglePassPostCallExitInsideASplice.class;
        Method entry = me.getMethod("entry", CountDownLatch.class);
        Method entrySelf = me.getMethod("entrySelf", boolean.class);

        // The fixed case first, cold.
        int coldBefore = Callee.value();
        int coldAfter = (Integer) entrySelf.invoke(null, true);
        System.out.println("cold-self first-after-new=" + (coldAfter != coldBefore));

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
        System.out.println("parked first-after-new=" + (p[1] != p[0]));

        long sum = 0;
        for (long k = 0; k < WARM; k++) {
            sum += (Integer) entrySelf.invoke(null, false);
        }
        sink = sum;
        int before = Callee.value();
        int after = (Integer) entrySelf.invoke(null, true);
        System.out.println("self first-after-new=" + (after != before));
    }
}
