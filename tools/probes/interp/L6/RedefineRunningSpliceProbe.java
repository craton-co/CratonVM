// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 23, lane L6: a COMPILED loop that is already
// running, and that inlined (spliced) a one-line callee, runs the callee's NEW
// bytecode from its first iteration after the callee's class is retransformed
// (docs/internal/fixed-bugs/interpreter-L6-a-running-splice-of-a-redefined-callee-runs-its-old-bytecode-FIXED-20260926.md).
//
// A worker spins in `spin`, whose loop calls `Callee.value()` (`sipush 12345;
// ireturn`, small enough for either tier to splice). Once the loop is hot
// (OSR-compiled, with the callee spliced), main retransforms `Callee` so that
// `value()` answers 12346, and only THEN publishes `phase = 1`. Every iteration
// that reads `phase == 1` therefore starts after the retransform returned, and
// must see the new value.
//
// HotSpot 25 prints (with the agent, JIT on or -Xint):
//     warm=true
//     after-old=0
//     after-new=true
//     value-now=12346
// HotSpot deoptimizes the running frame inside the redefinition's safepoint
// (Deoptimization::deoptimize_all_marked). CratonVM before wave 23 with the
// JIT on printed a large after-old= count (the running OSR body kept executing
// its splice of the old bytecode; the wave-22 not-entrant patch only redirects
// NEW entries). Since wave 23 the redefinition marks every stale body
// withdrawn (CompiledMethod::is_withdrawn_by_redefinition) and takes a
// handshake on the redefining thread; the loop's back-edge poll then leaves
// for the interpreter (jvmti_events.rs::polling_body_must_leave), whose next
// call reaches the new bytecode.
//
// Wave 24, lane L6: on the host the wave-23 build still printed
// `after-old=53015988 / after-new=false` with the JIT on, and no exit line at
// all. `spin` runs in an OPTIMIZING OSR body served by the VM's OSR memo; such
// a body is never published, so its reserved compile id is never bound and the
// safepoint slow path's `lookup_compile_id` found no body to judge
// (helpers.rs::jit_safepoint_loop_exit_verdict). It now names the body through
// the OSR door's continuation record (`running_osr_body_named`), and a
// whole-cache redefinition -- this one is, because the IR tier spliced
// `Callee` -- withdraws every body compiled before its flush, even one the memo
// no longer lists (`JitCache::body_withdrawn_by_redefinition`).
//
// POSITIVE CONTROL (JIT on, CRATONVM_DBG_DEOPT=1 CRATONVM_DBG_JITC=1), in this
// order after the `osr optimizing REUSE RedefineRunningSpliceProbe.spin pc=7`
// line:
//     [cratonvm-jitc] not-entrant pass: candidates=N patched=N Published=0 ...
//     [cratonvm-deopt] withdrawn body told to leave: RedefineRunningSpliceProbe.spin:()V verdict=0x3
//     [cratonvm-deopt] OSR-exit TRANSFER RedefineRunningSpliceProbe.spin()V entry_pc=7 resume_bci=7
// (resume_bci is the loop header, bci 7 of `spin`, the state a back-edge
// poll's mode exit carries). No TRANSFER line for `spin` = the fix did not
// engage. The N not-entrant candidates include JDK methods: the IR tier
// spliced `Callee`, which leaves no per-body record, so the redefinition takes
// the whole-cache path and patches every published body, as HotSpot
// deoptimizes every nmethod when dependencies are not recorded
// (docs/internal/fixed-bugs/interpreter-L1-proposal-per-body-copied-bytecode-dependencies-FIXED-20260930.md
// is what would scope it).
// Run with and without --nojit, and under --compatible and the default; the
// output must not change.
//
// Wave-37 regression (fixed in interpreter round i1 wave 38, lane L3): the
// wave-37 build printed `after-old=0 / after-new=false` in 10 of 30 JIT runs.
// The worker's forced exit poll ran `safepoint_check` while the retransforming
// thread still held the class-manager writer, so its frame conversion was
// deferred; after the OSR-exit TRANSFER its dispatch loop retried the
// conversion at every top through `obsolete_frames::convert_at_loop_top`,
// whose "no frame of a redefined class" shortcut never cleared the pending
// mark, and ran no bytecode for the whole retry bound (seconds), past main's
// 200 ms. Positive control:
//     CRATONVM_DBG_RETRANSFORM=1 CRATONVM_DBG_DEOPT=1 CRATONVM_DBG_JITC=1
// After the TRANSFER line a run whose conversion was deferred prints
//     [redefine] deferred conversion done after <n> retries: thread <id>
// with a small n (the writer's hold); a
//     [redefine] deferred conversion GIVEN UP after <n> retries in <ms> ms
// line is the regression. Runs that polled after the writer was released
// print neither.
//
// SETUP: needs a java agent. Build a jar whose manifest has
//     Premain-Class: RedefineRunningSpliceProbe$Agent
//     Can-Retransform-Classes: true
// containing RedefineRunningSpliceProbe*.class, then run
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar RedefineRunningSpliceProbe
// Without the agent both VMs print warm=true, then "no agent" and
// value-now=12345.
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;

public class RedefineRunningSpliceProbe {
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

    /** Rewrites `sipush 12345; ireturn` (11 30 39 AC) to `sipush 12346`. */
    static final class Bump implements ClassFileTransformer {
        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (!"RedefineRunningSpliceProbe$Callee".equals(className)) {
                return null;
            }
            byte[] out = bytes.clone();
            for (int i = 0; i + 3 < out.length; i++) {
                if (out[i] == 0x11 && out[i + 1] == 0x30 && out[i + 2] == 0x39
                        && out[i + 3] == (byte) 0xAC) {
                    out[i + 2] = 0x3A;
                }
            }
            return out;
        }
    }

    static final int OLD = 12345;
    static volatile boolean stop;
    static volatile int phase;
    static volatile long warm;
    static volatile long[] result;

    static void spin() {
        long before = 0;
        long oldAfter = 0;
        long newAfter = 0;
        while (!stop) {
            int p = phase;
            int v = Callee.value();
            if (p == 0) {
                if ((++before & 0xFFFF) == 0) {
                    warm = before;
                }
            } else if (v == OLD) {
                oldAfter++;
            } else {
                newAfter++;
            }
        }
        result = new long[] {before, oldAfter, newAfter};
    }

    public static void main(String[] args) throws Exception {
        Thread worker = new Thread(RedefineRunningSpliceProbe::spin);
        worker.start();
        long deadline = System.nanoTime() + 60_000_000_000L;
        while (warm < 4_000_000L && System.nanoTime() < deadline) {
            Thread.sleep(1);
        }
        System.out.println("warm=" + (warm >= 4_000_000L));
        Instrumentation i = inst;
        boolean agent = i != null && i.isRetransformClassesSupported();
        if (agent) {
            i.addTransformer(new Bump(), true);
            i.retransformClasses(Callee.class);
        }
        phase = 1;
        Thread.sleep(200);
        stop = true;
        worker.join();
        long[] r = result;
        if (agent) {
            System.out.println("after-old=" + r[1]);
            System.out.println("after-new=" + (r[2] > 0));
        } else {
            System.out.println("no agent");
        }
        System.out.println("value-now=" + Callee.value());
    }
}
