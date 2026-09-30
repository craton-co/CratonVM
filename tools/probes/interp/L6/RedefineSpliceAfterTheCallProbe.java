// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 26, lane L6: the rest of the iteration. A
// COMPILED loop that is inside a call while a class it spliced is
// retransformed must run the NEW bytecode for a splice that comes AFTER that
// call in the SAME iteration
// (docs/known-issues/interpreter/i24-L6-a-compiled-frame-in-a-call-at-the-redefinition-keeps-its-old-splice-20260927.md,
// "What remains", first item).
//
// RedefineSpliceAcrossACallProbe checks the iterations after the call; there
// the splice comes BEFORE the call in the loop body, so the loop's first
// back edge after the call (where wave 25's forced exit polls send the frame
// to the interpreter) comes before the splice runs again. Here the splice is
// the next thing the iteration does after the call returns.
//
// Two shapes, each a loop hot enough to be OSR-compiled, which every 1024
// iterations makes one call and then reads `Callee.value()` (`sipush 12345;
// ireturn`, spliced) -- with no branch between the call and the read that
// could have been compiled as an unreached-code trap, so the call and the
// splice after it are on the compiled path long before the one iteration
// that matters:
//   parked  a worker's call is `LATCHES[idx].await()`: LATCHES[0] is open,
//           LATCHES[1] (idx == 1 only at iteration WARM, computed without a
//           branch) is released by main after it retransformed Callee.
//   self    main's own call is `inst.retransformClasses(ARGS[idx])`: ARGS[0]
//           is the empty array (the JDK returns at once), ARGS[1] names
//           Callee.
// `first-after-new` says whether the read right after the WARM call saw the
// retransformed value.
//
// HotSpot 25 (agent, JIT on and -Xint alike; JDK 25.0.3 locally):
//     parked first-after-new=true
//     self first-after-new=true
// Without the agent: `no agent`. HotSpot deoptimizes every frame that depends
// on the redefined class at the redefinition's safepoint, so the call returns
// into the interpreter at the invoke's successor (the frame's return address
// is patched to the deopt blob), and the read runs the new bytecode.
//
// CratonVM, expected before a fix (not run by the lane that filed this):
// `--nojit` prints HotSpot's lines (the interpreter reads the new method on
// its next call). With the JIT on, `first-after-new=false` on either row
// whose loop was compiled with the splice and was in compiled code at the
// call: a withdrawn body leaves for the interpreter only at an exit-capable
// back-edge poll (forced since wave 25), and the read after the call comes
// first. POSITIVE CONTROL: CRATONVM_DBG_JITC=1 prints `exit polls forced:
// bodies=N` (N >= 1) after each retransform, and CRATONVM_DBG_DEOPT=1 prints
// `withdrawn body told to leave: RedefineSpliceAfterTheCallProbe.parked...`
// (`.self...`) only AFTER the read -- the frame left at the back edge that
// follows it. A row that prints `true` with the JIT on and no such line was
// not running compiled code at the call (check `CRATONVM_DBG_JITC`'s OSR
// lines for the loop).
//
// Wave 27 (lane L6): the single-pass tier's OSR compiles put a patchable
// post-call exit after each real call (`op_invoke.rs::emit_post_call_exit_site`),
// which the redefinition forces with the exit polls, so a loop running a
// SINGLE-PASS OSR body leaves at the call's successor. Expected now, with
// `CRATONVM_JIT_OSR_OPTIMIZING=0` (every OSR body single-pass): HotSpot's two
// lines, in the default mode and `--compatible`; `CRATONVM_DBG_JITC=1` prints
// `exit polls candidate: ...parked... post-call-sites=N` (N >= 1) and
// `CRATONVM_DBG_DEOPT=1` `withdrawn body told to leave: ...parked...` BEFORE
// the read. With the optimizing OSR tier (the default) a row may still print
// `false`: that tier has no post-call site yet (stage 3 of
// `i26-L6-proposal-a-patchable-post-call-exit-for-withdrawn-bodies-20260928.md`).
// HotSpot re-run 2026-09-27 (JDK 25.0.3, JIT and -Xint): the two lines above.
//
// Wave 28 (lane L6), CORRECTION: under `CRATONVM_JIT_OSR_OPTIMIZING=0` this
// probe is NOT a positive control for the single-pass post-call exit. A
// single-pass OSR compile splices no callee (`jit_bridge.rs::compile_osr_body`
// passes the backend an empty `inline_sites` map), so there `Callee.value()`
// is a real call that reaches the new bytecode by itself; the body copies
// nothing of `Callee`, the retransform does not withdraw it, and both rows
// print `true` whether or not any exit fires -- which is what the wave-27
// host run saw (no `exit polls candidate` line, and the wave-26 binary
// printing `true` too; its `tier=C2 optimized=true osr_bci=10` line is the
// background TASK's tier, not the body's). The single-pass control is
// `RedefineElidedCtorAfterTheCallProbe` (an elided constructor is the one
// thing such a body copies). With the DEFAULT optimizing OSR tier this probe
// does test something: that tier splices `Callee.value()`, and since wave 28
// its OSR-door compiles have post-call exits too (`ir_lower.rs::
// maybe_emit_post_call_exit_site`) -- but only after a REAL call, and
// `await()` / `retransformClasses` have no exception table, so that tier may
// splice them, and a call made inside a splice gets no site. A row may
// therefore still print `false` by default; `RedefineElidedCtorAfterTheCallProbe`
// wraps its calls in methods with an exception table to keep them real calls.
// When a site fires, `CRATONVM_DBG_DEOPT=1` shows `post-call exit verdict #k:
// RedefineSpliceAfterTheCallProbe.parked... named=true withdrawn=true
// verdict=0x..` (non-zero).
//
// SETUP: a jar whose manifest has
//     Premain-Class: RedefineSpliceAfterTheCallProbe$Agent
//     Can-Retransform-Classes: true
// containing RedefineSpliceAfterTheCallProbe*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar RedefineSpliceAfterTheCallProbe
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;
import java.util.concurrent.CountDownLatch;

public class RedefineSpliceAfterTheCallProbe {
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
            if (!"RedefineSpliceAfterTheCallProbe$Callee".equals(className)) {
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
    static volatile int[] parkedResult;

    /** 1 at `i == WARM`, else 0, without a branch (0 <= i, WARM < 2^62). */
    static int at(long i) {
        return (int) (((i ^ WARM) - 1) >>> 63);
    }

    /** {value before the loop, value read right after the WARM call}. */
    static int[] parked() throws InterruptedException {
        int before = Callee.value();
        int[] firsts = new int[2];
        for (long i = 0; i < WARM + AFTER; i++) {
            if ((i & 1023) == 0) {
                int idx = at(i);
                parkedIn |= idx == 1;
                LATCHES[idx].await();
                firsts[idx] = Callee.value();
            }
        }
        return new int[] {before, firsts[1]};
    }

    static int[] self(Instrumentation inst) throws Exception {
        Class<?>[][] args = {new Class<?>[0], new Class<?>[] {Callee.class}};
        int before = Callee.value();
        int[] firsts = new int[2];
        for (long k = 0; k < WARM + AFTER; k++) {
            if ((k & 1023) == 0) {
                int idx = at(k);
                inst.retransformClasses(args[idx]);
                firsts[idx] = Callee.value();
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
        Thread worker = new Thread(() -> {
            try {
                parkedResult = parked();
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
        });
        worker.start();
        while (!parkedIn) {
            Thread.sleep(1);
        }
        // Parked in `GO.await()`, a call made from inside its compiled loop.
        Thread.sleep(50);
        i.retransformClasses(Callee.class);
        GO.countDown();
        worker.join();
        int[] p = parkedResult;
        System.out.println("parked first-after-new=" + (p[1] != p[0]));
        int[] s = self(i);
        System.out.println("self first-after-new=" + (s[1] != s[0]));
    }
}
