// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 60, lane L2: item 2 of
// docs/internal/fixed-bugs/interpreter-L6-a-compiled-frame-in-a-call-at-the-redefinition-keeps-its-old-splice-FIXED-20261007.md
// -- a compiled frame that is inside a call when a class it spliced is
// retransformed, where the call is the LAST instruction of an `if` arm, so
// its successor is the JOIN (a branch target). The read of the spliced
// `Callee.value()` at the join, right after the call returns, must run the
// NEW bytecode (HotSpot returns into the interpreter at the call's
// successor).
//
// Rows (the cold one first, then again warm):
//   cold-self    `joinSelf(true, true)` once, cold (interpreted);
//   parked       a worker warms `joinEntry(OPEN, k & 1)` up (a method-entry
//                body, reached through reflection only), then
//                `joinEntry(GO, true)` parks in `GO.await()` while main
//                retransforms Callee;
//   self         `joinSelf(false, k & 1)` warmed up, then
//                `joinSelf(true, true)`, which retransforms Callee on the
//                calling thread itself;
//   parked-osr   the same shape inside a hot loop (an OSR body): every 1024
//                iterations `parkOn(idx)` as the `if` arm's last call, then
//                `Callee.value()` at the join;
//   self-osr     the same loop on main, whose arm's call retransforms.
// The calls (`block`, `retransform`, `parkOn`, `retransformNow`) carry an
// exception table so the optimizing tier keeps them real calls. The arm is
// taken on every other warm-up call so that no tier compiles it as an
// unreached-code trap.
//
// HOTSPOT_EXPECTED_BEGIN (Temurin 25.0.3 with the agent, default flags and -Xint)
// cold-self first-after-new=true
// parked first-after-new=true
// self first-after-new=true
// parked-osr first-after-new=true
// self-osr first-after-new=true
// HOTSPOT_EXPECTED_END
// Without the agent both VMs print `no agent`.
//
// CratonVM (lane's Windows debug build, base 8004f6b7e1, measured):
// * base, default flags: `parked first-after-new=false` (its `joinEntry`
//   runs a single-pass body: the optimizing compile is kept no better), the
//   other rows HotSpot's;
// * base, `CRATONVM_TIER_C2_THRESHOLD=1000000000` (single-pass method-entry
//   bodies): `parked` and `self` print `false`;
// * base, plus `CRATONVM_JIT_OSR_OPTIMIZING=0` (single-pass OSR bodies too):
//   all four warm rows print `false`;
// * `--nojit`: HotSpot's lines.
// The cause: a single-pass post-call exit claim refused every successor that
// is a branch target (`op_invoke.rs::emit_post_call_exit_claim`), and the
// join of an `if` with no `else` is one. With
// `CRATONVM_JIT_POST_CALL_EXITS_AT_FORWARD_JOINS` on (default ON since the
// wave-60 lane's switch-on commit): HotSpot's lines in all of the modes
// above and `--compatible`. POSITIVE CONTROL, `CRATONVM_DBG_DEOPT=1`:
//   `post-call exit claim at bci=8 of L2W60PostCallExitAtAJoin.joinEntry:(Ljava/util/concurrent/CountDownLatch;Z)I: Own (points there: [])`
// (and the same for `joinSelf`); with the switch `=0` no such line, and the
// rows above print `false` again.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L2W60PostCallExitAtAJoin$Agent
//     Can-Retransform-Classes: true
// containing L2W60PostCallExitAtAJoin*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L2W60PostCallExitAtAJoin
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.lang.reflect.Method;
import java.security.ProtectionDomain;
import java.util.concurrent.CountDownLatch;

public class L2W60PostCallExitAtAJoin {
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
            if (!"L2W60PostCallExitAtAJoin$Callee".equals(className)) {
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
    /** A multiple of 1024, so the loop rows' call at iteration LOOP_WARM is made. */
    static final long LOOP_WARM = 20L << 20;
    static final long LOOP_AFTER = 2L << 20;
    static volatile boolean parkedIn;
    static volatile boolean loopParkedIn;
    static final CountDownLatch GO = new CountDownLatch(1);
    static final CountDownLatch GO_LOOP = new CountDownLatch(1);
    static final CountDownLatch OPEN = new CountDownLatch(0);
    static final CountDownLatch[] LATCHES = {OPEN, GO_LOOP};
    static volatile int[] parkedResult;
    static volatile int[] loopParkedResult;
    static volatile long sink;
    static final Class<?>[][] ARGS = {new Class<?>[0], new Class<?>[] {Callee.class}};

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

    /** The loop rows' parked call. */
    static void parkOn(int idx) {
        try {
            if (idx == 1) {
                loopParkedIn = true;
            }
            LATCHES[idx].await();
        } catch (InterruptedException e) {
            throw new RuntimeException(e);
        }
    }

    /** The loop rows' self call. */
    static void retransformNow(int idx) {
        try {
            inst.retransformClasses(ARGS[idx]);
        } catch (Exception e) {
            throw new RuntimeException(e);
        }
    }

    /** Reached through reflection only. The call's successor is the join. */
    public static int joinEntry(CountDownLatch latch, boolean call) {
        if (call) {
            block(latch);
        }
        return Callee.value();
    }

    /** Reached through reflection only, as `joinEntry`. */
    public static int joinSelf(boolean now, boolean call) {
        if (call) {
            retransform(now);
        }
        return Callee.value();
    }

    /** 1 at `i == LOOP_WARM`, else 0, without a branch. */
    static int at(long i) {
        return (int) (((i ^ LOOP_WARM) - 1) >>> 63);
    }

    /** {value before the loop, value read at the join right after the LOOP_WARM call}. */
    static int[] parkedLoop() {
        int before = Callee.value();
        int[] firsts = new int[2];
        for (long i = 0; i < LOOP_WARM + LOOP_AFTER; i++) {
            int idx = at(i);
            if ((i & 1023) == 0) {
                parkOn(idx);
            }
            firsts[idx] = Callee.value();
        }
        return new int[] {before, firsts[1]};
    }

    static int[] selfLoop() {
        int before = Callee.value();
        int[] firsts = new int[2];
        for (long i = 0; i < LOOP_WARM + LOOP_AFTER; i++) {
            int idx = at(i);
            if ((i & 1023) == 0) {
                retransformNow(idx);
            }
            firsts[idx] = Callee.value();
        }
        return new int[] {before, firsts[1]};
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRetransformClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        i.addTransformer(new Bump(), true);
        Class<?> me = L2W60PostCallExitAtAJoin.class;
        Method joinEntry = me.getMethod("joinEntry", CountDownLatch.class, boolean.class);
        Method joinSelf = me.getMethod("joinSelf", boolean.class, boolean.class);

        // The fixed case first, cold.
        int coldBefore = Callee.value();
        int coldAfter = (Integer) joinSelf.invoke(null, true, true);
        System.out.println("cold-self first-after-new=" + (coldAfter != coldBefore));

        Thread worker = new Thread(() -> {
            try {
                long sum = 0;
                for (long k = 0; k < WARM; k++) {
                    sum += (Integer) joinEntry.invoke(null, OPEN, (k & 1) == 0);
                }
                sink = sum;
                int before = Callee.value();
                parkedResult = new int[] {before, (Integer) joinEntry.invoke(null, GO, true)};
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
        GO.countDown();
        worker.join();
        int[] p = parkedResult;
        System.out.println("parked first-after-new=" + (p[1] != p[0]));

        long sum = 0;
        for (long k = 0; k < WARM; k++) {
            sum += (Integer) joinSelf.invoke(null, false, (k & 1) == 0);
        }
        sink = sum;
        int selfBefore = Callee.value();
        int selfAfter = (Integer) joinSelf.invoke(null, true, true);
        System.out.println("self first-after-new=" + (selfAfter != selfBefore));

        Thread loopWorker = new Thread(() -> loopParkedResult = parkedLoop());
        loopWorker.start();
        while (!loopParkedIn) {
            Thread.sleep(1);
        }
        Thread.sleep(50);
        i.retransformClasses(Callee.class);
        GO_LOOP.countDown();
        loopWorker.join();
        int[] lp = loopParkedResult;
        System.out.println("parked-osr first-after-new=" + (lp[1] != lp[0]));
        int[] ls = selfLoop();
        System.out.println("self-osr first-after-new=" + (ls[1] != ls[0]));
    }
}
