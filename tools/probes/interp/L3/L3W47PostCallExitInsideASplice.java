// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 47, lane L3 -- the positive control of the first
// stage of
// docs/internal/fixed-bugs/i39-L3-proposal-post-call-exits-inside-a-splice-FIXED-20261007.md:
// a compiled loop that SPLICED a small callee `step` and is inside a REAL call
// made from within that splice when a class the splice copied is
// retransformed. HotSpot's deopt returns into the interpreter at the call's
// successor, in `step`'s own frame, so the rest of `step` -- here the read of
// `Callee.value()` right after the call -- runs the NEW bytecode.
//
// Two shapes, each a loop hot enough to be OSR-compiled at the optimizing
// tier, which every 1024 iterations calls `step(idx)`:
//   parked  `step` calls `block(LATCHES[idx])` (a real call: `block` has an
//           exception table, which the inline resolver refuses), whose
//           `await()` parks the worker at iteration WARM until main has
//           retransformed Callee;
//   self    `stepSelf` calls `retransform(ARGS[idx])` (a real call for the
//           same reason), which retransforms Callee at iteration WARM.
// Then `step` / `stepSelf` return `Callee.value()` (`sipush 12345; ireturn`,
// spliced into them and so into the loop). `first-after-new` says whether
// that read saw the retransformed value.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3 with the agent, JIT and -Xint alike)
// parked first-after-new=true
// self first-after-new=true
// HOTSPOT_EXPECTED_END
// Without the agent: `no agent`.
//
// CratonVM on the base a451e608e (read from the code): with the JIT on and
// the loop in its optimizing OSR body, `first-after-new=false` on a row whose
// `step` was spliced: the call inside the splice got no post-call exit
// (`ir_lower.rs::post_call_exit_state` refused "a call inside a splice"), so
// the frame ran the rest of the splice and of its iteration on the old
// `Callee.value()` and left at the next back edge. `--nojit` prints HotSpot's
// lines.
//
// Since wave 47 (lane L3), under `CRATONVM_JIT_IR_POST_CALL_SPLICE_EXITS`
// (default on in the lane's last commit): the builder records a chain snapshot
// at the successor of each real call inside a splice, the OSR-door lowering
// puts a post-call exit there whose frame is that chain (`step` at its bci
// after the call, over the loop parked in its `invoke` of `step`), the
// redefinition forces it, and the VM's planless chain transfer rebuilds the
// two frames. Expected: HotSpot's lines, default and `--compatible`.
//
// POSITIVE CONTROL: `CRATONVM_DBG_IR_COMPILES=1` (or `CRATONVM_DBG_JITC=1`)
// prints `[ir] post-call-chain-sites: N of M spliced call(s) ...` with N >= 1
// for the OSR compile of `parked` / `self`; `CRATONVM_DBG_DEOPT=1` prints a
// `post-call exit verdict #k: L3W47PostCallExitInsideASplice.parked... named=true
// withdrawn=true verdict=0x..` (non-zero) line, then `OSR guard-exit CHAIN
// transfer: live frame @.., 1 frame(s) pushed, trapping bci=..`. With
// `CRATONVM_JIT_IR_POST_CALL_SPLICE_EXITS=0` the first line is absent and a
// row whose `step` was spliced prints `false`. A row that prints `true` with
// no such lines did not run a spliced `step` at the call (check the OSR lines
// of `CRATONVM_DBG_JITC`).
//
// SETUP: a jar whose manifest has
//     Premain-Class: L3W47PostCallExitInsideASplice$Agent
//     Can-Retransform-Classes: true
// containing L3W47PostCallExitInsideASplice*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W47PostCallExitInsideASplice
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;
import java.util.concurrent.CountDownLatch;

public class L3W47PostCallExitInsideASplice {
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
            if (!"L3W47PostCallExitInsideASplice$Callee".equals(className)) {
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

    /** A multiple of 1024, so the call at iteration WARM is made. */
    static final long WARM = 20L << 20;
    static final long AFTER = 2L << 20;
    static volatile boolean parkedIn;
    static final CountDownLatch GO = new CountDownLatch(1);
    static final CountDownLatch OPEN = new CountDownLatch(0);
    static final CountDownLatch[] LATCHES = {OPEN, GO};
    static final Class<?>[][] ARGS = {new Class<?>[0], new Class<?>[] {Callee.class}};
    static volatile int[] parkedResult;

    /** 1 at `i == WARM`, else 0, without a branch (0 <= i, WARM < 2^62). */
    static int at(long i) {
        return (int) (((i ^ WARM) - 1) >>> 63);
    }

    /** A real call: its exception table keeps it out of every splice. */
    static void block(CountDownLatch latch) {
        try {
            latch.await();
        } catch (InterruptedException e) {
            throw new RuntimeException(e);
        }
    }

    /** A real call, for the same reason. */
    static void retransform(Class<?>[] classes) {
        try {
            inst.retransformClasses(classes);
        } catch (Exception e) {
            throw new RuntimeException(e);
        }
    }

    /** Spliced into `parked`: a real call, then the splice of Callee. */
    static int step(int idx) {
        block(LATCHES[idx]);
        return Callee.value();
    }

    /** Spliced into `self`: a real call, then the splice of Callee. */
    static int stepSelf(int idx) {
        retransform(ARGS[idx]);
        return Callee.value();
    }

    /** {value before the loop, value read right after the WARM call}. */
    static int[] parked() {
        int before = Callee.value();
        int[] firsts = new int[2];
        for (long i = 0; i < WARM + AFTER; i++) {
            if ((i & 1023) == 0) {
                int idx = at(i);
                parkedIn |= idx == 1;
                firsts[idx] = step(idx);
            }
        }
        return new int[] {before, firsts[1]};
    }

    static int[] self() {
        int before = Callee.value();
        int[] firsts = new int[2];
        for (long k = 0; k < WARM + AFTER; k++) {
            if ((k & 1023) == 0) {
                int idx = at(k);
                firsts[idx] = stepSelf(idx);
            }
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
        Thread worker = new Thread(() -> parkedResult = parked());
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
        int[] s = self();
        System.out.println("self first-after-new=" + (s[1] != s[0]));
    }
}
