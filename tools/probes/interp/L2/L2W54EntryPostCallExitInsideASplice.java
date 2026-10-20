// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 54, lane L2 -- the positive control of stage 2 of
// docs/internal/fixed-bugs/i39-L3-proposal-post-call-exits-inside-a-splice-FIXED-20261007.md
// (`CRATONVM_JIT_IR_ENTRY_POST_CALL_SPLICE_EXITS`): a METHOD-ENTRY compiled
// body that SPLICED a small callee `step` is inside a REAL call made from
// within that splice when a class the splice copied is retransformed.
// HotSpot's deopt returns into the interpreter at the call's successor, in
// `step`'s own frame, so the rest of `step` -- the read of `Callee.value()`
// right after the call -- runs the NEW bytecode. Wave 47's probe
// (`L3/L3W47PostCallExitInsideASplice`) is the OSR-door twin.
//
// `entry` / `entrySelf` are called through reflection only, so no compiled
// caller splices them: each runs as its own method-entry body (single-pass
// first, the optimizing tier once it superseded it), which splices `step` /
// `stepSelf`. Rows:
//   cold-self  `entrySelf(1)` once, cold (interpreted): `stepSelf` retransforms
//              Callee and then reads it;
//   parked     a worker warms `entry(0)` up, then `entry(1)` parks in
//              `GO.await()` (called from `block`, a real call: its exception
//              table keeps it out of every splice) while main retransforms
//              Callee;
//   self       `entrySelf(0)` warmed up, then `entrySelf(1)`, whose
//              `retransform` call (a real call, for the same reason)
//              retransforms Callee on the calling thread itself.
// `first-after-new` says whether the read after the call saw the
// retransformed value.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3 with the agent, default flags; the same
// with -Xint)
// cold-self first-after-new=true
// parked first-after-new=true
// self first-after-new=true
// HOTSPOT_EXPECTED_END
// Without the agent both VMs print `no agent`.
//
// CratonVM on the base d226deb8fd (read from the code): with the JIT on and
// `entry` running its OPTIMIZING method-entry body, `parked` and `self` print
// `first-after-new=false`: the call inside the splice got no post-call exit in
// a method-entry compile (`ir_lower.rs::post_call_chain_exit_state` admitted
// OSR-door compiles only), so the activation ran the rest of the splice on the
// old `Callee.value()`. `--nojit` prints HotSpot's lines. A row whose body was
// still the single-pass one prints `false` too (single-pass splices get no
// site: the proposal's single-pass stage).
//
// POSITIVE CONTROL: `CRATONVM_DBG_IR_COMPILES=1` (or `CRATONVM_DBG_JITC=1`)
// prints `[ir] post-call-chain-sites: N of M spliced call(s) ...` with N >= 1
// for the method-entry compile of `entry` / `entrySelf`; `CRATONVM_DBG_DEOPT=1`
// prints a `post-call exit verdict #k: L2W54EntryPostCallExitInsideASplice.entry...
// named=true withdrawn=true verdict=0x..` (non-zero) line and then the door's
// chain resume (`the redefined spliced callees resume in the bytecode the
// compile spliced`). With `CRATONVM_JIT_IR_ENTRY_POST_CALL_SPLICE_EXITS=0` no
// method-entry `post-call-chain-sites` line has N >= 1 and a row whose
// optimizing body spliced `step` prints `false`. A `post-call exit verdict ...
// could not be rebuilt now; the frame stays` line means the VM's resumability
// check refused (the spliced body's constants did not translate).
//
// SETUP: a jar whose manifest has
//     Premain-Class: L2W54EntryPostCallExitInsideASplice$Agent
//     Can-Retransform-Classes: true
// containing L2W54EntryPostCallExitInsideASplice*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L2W54EntryPostCallExitInsideASplice
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.lang.reflect.Method;
import java.security.ProtectionDomain;
import java.util.concurrent.CountDownLatch;

public class L2W54EntryPostCallExitInsideASplice {
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
            if (!"L2W54EntryPostCallExitInsideASplice$Callee".equals(className)) {
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

    static final long WARM = 1L << 21;
    static volatile boolean parkedIn;
    static final CountDownLatch GO = new CountDownLatch(1);
    static final CountDownLatch OPEN = new CountDownLatch(0);
    static final CountDownLatch[] LATCHES = {OPEN, GO};
    static final Class<?>[][] ARGS = {new Class<?>[0], new Class<?>[] {Callee.class}};
    static volatile int[] parkedResult;
    static volatile long sink;

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
            if (classes.length > 0) {
                inst.retransformClasses(classes);
            }
        } catch (Exception e) {
            throw new RuntimeException(e);
        }
    }

    /** Spliced into `entry`: a real call, then the splice of Callee. */
    static int step(int idx) {
        block(LATCHES[idx]);
        return Callee.value();
    }

    /** Spliced into `entrySelf`: a real call, then the splice of Callee. */
    static int stepSelf(int idx) {
        retransform(ARGS[idx]);
        return Callee.value();
    }

    /** Reached through reflection only: a method-entry body of its own. */
    public static int entry(int idx) {
        if (idx == 1) {
            parkedIn = true;
        }
        return step(idx);
    }

    /** Reached through reflection only, as `entry`. */
    public static int entrySelf(int idx) {
        return stepSelf(idx);
    }

    static int call(Method m, int idx) throws Exception {
        return (Integer) m.invoke(null, idx);
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRetransformClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        i.addTransformer(new Bump(), true);
        Method entry = L2W54EntryPostCallExitInsideASplice.class.getMethod("entry", int.class);
        Method entrySelf = L2W54EntryPostCallExitInsideASplice.class.getMethod("entrySelf", int.class);

        // The fixed case first, cold.
        int coldBefore = Callee.value();
        int coldAfter = call(entrySelf, 1);
        System.out.println("cold-self first-after-new=" + (coldAfter != coldBefore));

        Thread worker = new Thread(() -> {
            try {
                int before = Callee.value();
                long sum = 0;
                for (long k = 0; k < WARM; k++) {
                    sum += call(entry, 0);
                }
                sink = sum;
                parkedResult = new int[] {before, call(entry, 1)};
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

        int before = Callee.value();
        long sum = 0;
        for (long k = 0; k < WARM; k++) {
            sum += call(entrySelf, 0);
        }
        sink = sum;
        int after = call(entrySelf, 1);
        System.out.println("self first-after-new=" + (after != before));
    }
}
