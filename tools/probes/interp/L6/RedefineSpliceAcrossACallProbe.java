// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 24, lane L6: a COMPILED loop that spliced a
// one-line callee, and that is INSIDE A CALL while the callee's class is
// retransformed, must run the callee's NEW bytecode from its first iteration
// after that call returns
// (docs/known-issues/interpreter/i24-L6-a-compiled-frame-in-a-call-at-the-redefinition-keeps-its-old-splice-20260927.md).
//
// Two shapes, each a loop hot enough to be OSR-compiled with `Callee.value()`
// (`sipush 12345; ireturn`) spliced into it, which at iteration WARM makes one
// call and then keeps looping:
//   parked  a worker's loop calls `go.await()` once; main retransforms Callee
//           while the worker is parked in that call, then releases it.
//   self    main's own loop calls `retransformClasses` itself.
// Every iteration after the call must see the new value (12346).
//
// HotSpot 25 prints (agent, JIT on and -Xint alike):
//     parked after-old=0 after-new=true
//     self after-old=0 after-new=true
//     value-now=12348
// (Callee is loaded after the transformer is added, so its load is the first
// of the three transforms.) Without the agent: `no agent`, `value-now=12345`.
// HotSpot deoptimizes every frame that depends on the redefined class at the
// redefinition's safepoint, including one whose thread is blocked in a call
// (the frame is patched to return into the deopt blob) and the redefining
// thread's own.
//
// CratonVM (not run by the lane that filed this): a withdrawn body leaves for
// the interpreter only at a back-edge poll's SLOW path
// (jvmti_events.rs::withdrawn_body_may_leave, wave 23/24), which a thread
// reaches during the redefinition's loop-exit handshake. A thread blocked in
// a call is not polling then, and the redefining thread is not asked at all,
// so both loops keep running the old splice after the call returns, until
// some later safepoint request sends them through the slow path: expected
// after-old=N (large) for both rows with the JIT on, 0 with --nojit.
// POSITIVE CONTROL: CRATONVM_DBG_DEOPT=1 prints NO `OSR-exit TRANSFER
// RedefineSpliceAcrossACallProbe.parked` / `.self` line right after the
// retransform while the bug stands; it prints one per loop once fixed.
//
// Wave 24 host run: differs in every JIT mode (after-old large on both rows).
// Wave 25 (lane L6): the redefinition FORCES the exit polls of every body it
// withdrew (JitCache::force_withdrawn_exit_polls): each exit-capable back-edge
// poll of such a body asks the safepoint slow path whatever the
// stop-the-world flag holds, so both loops leave for the interpreter at their
// first back edge after the call returns. Expected on CratonVM now, JIT on,
// default and --compatible: exactly HotSpot's three lines. POSITIVE CONTROL:
// CRATONVM_DBG_JITC=1 prints `[cratonvm-jitc] exit polls forced: bodies=N`
// with N >= 1 after each retransform, and CRATONVM_DBG_DEOPT=1 prints one
// `withdrawn body told to leave: RedefineSpliceAcrossACallProbe.parked...`
// (and `.self...`) line right after its call returns. `bodies=0 ... spared=`
// or `refused=` non-zero means the loop's body was not forced (its own class
// redefined, or a poll shape the check did not accept).
//
// Wave 25 host run (merged build 198e98b95), JIT, default mode:
//     parked after-old=1996998 after-new=false
//     self after-old=1995998 after-new=false
//     value-now=12347
// (--nojit: after-old=0 after-new=true on both rows, value-now=12347 too).
// About 3,000 / 4,000 iterations after the call saw the new value and every
// later one the OLD value: the frame left compiled code once, ran
// interpreted until its back-edge counter reached the OSR door again, and was
// then handed pre-redefinition code. The wave-25 follow-up (lane L6b) makes
// every step of that visible under CRATONVM_DBG_JITC=1:
//   `exit polls forced: ... (candidates=N not-withdrawn=.. no-sites=..
//    forced-before=..) redefined=...`  on every redefinition, zeros included;
//   `exit polls candidate: <label> id=.. osr=true ir=.. withdrawn=.. sites=..`
//    for every OSR body the redefinition saw;
//   `OSR entry REFUSED (withdrawn by a redefinition): <label>` and
//   `OSR entry after a redefinition: <label> id=.. install_epoch=..` at the
//    OSR door, which name the body the loop re-entered;
//   `osr optimizing memo FORGETS a withdrawn body: <label>`.
// Second follow-up (lane L6c): the re-entered body (id=12 / id=17, compiled
// after the retransform) was not stale: the optimizing OSR entry seeds a
// header-live CONSTANT from the compile (`const_seeds`), so the frame's
// `before` was replaced by the NEW spliced value and every iteration compared
// new with new. The OSR door now refuses a body that copied a class
// redefined after the frame began (`OSR entry REFUSED (the frame predates a
// redefinition of a class the body copied)`); expected now: HotSpot's lines.
// `value-now=12347` (HotSpot 12348) is not this probe's JIT question: the
// transformer bumps once per call, and one call fewer than HotSpot means the
// initial load of Callee did not run the transformer (Callee loaded before
// `addTransformer`, or the load hook skipped a retransform-capable
// transformer); the retransform-twice issue (i25-L3) adds a base swap but no
// transformer call.
// Wave 26 (lane L3): it was the load hook. `parked()`'s first
// `invokestatic Callee.value()` loaded Callee through dispatch_static.rs's
// flat `load_class_concurrent`, which never offers a class to the chain; it now
// goes through `SharedVm::load_class_transformed`, and the retransformation base
// is the class file before Bump's load-time rewrite (as JVMTI caches it).
// Expected now: `value-now=12348` in every mode
// (docs/internal/fixed-bugs/interpreter-L3-a-retransform-probe-sees-one-transform-fewer-than-hotspot-FIXED-20260928.md).
//
// SETUP: a jar whose manifest has
//     Premain-Class: RedefineSpliceAcrossACallProbe$Agent
//     Can-Retransform-Classes: true
// containing RedefineSpliceAcrossACallProbe*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar RedefineSpliceAcrossACallProbe
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;
import java.util.concurrent.CountDownLatch;

public class RedefineSpliceAcrossACallProbe {
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
     * the n-th retransform (a retransform starts again from the original
     * bytes).
     */
    static final class Bump implements ClassFileTransformer {
        int bumps;

        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (!"RedefineSpliceAcrossACallProbe$Callee".equals(className)) {
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

    static final long WARM = 20_000_000L;
    static final long AFTER = 2_000_000L;
    static volatile boolean parkedIn;
    static final CountDownLatch GO = new CountDownLatch(1);
    static volatile long[] parkedResult;

    /** Returns {old, new} counts after the call at iteration WARM. */
    static long[] parked() throws InterruptedException {
        int before = Callee.value();
        long old = 0;
        long fresh = 0;
        for (long i = 0; i < WARM + AFTER; i++) {
            int v = Callee.value();
            if (i == WARM) {
                parkedIn = true;
                GO.await();
            } else if (i > WARM) {
                if (v == before) {
                    old++;
                } else {
                    fresh++;
                }
            }
        }
        return new long[] {old, fresh};
    }

    static long[] self(Instrumentation i) throws Exception {
        int before = Callee.value();
        long old = 0;
        long fresh = 0;
        for (long k = 0; k < WARM + AFTER; k++) {
            int v = Callee.value();
            if (k == WARM) {
                i.retransformClasses(Callee.class);
            } else if (k > WARM) {
                if (v == before) {
                    old++;
                } else {
                    fresh++;
                }
            }
        }
        return new long[] {old, fresh};
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRetransformClassesSupported()) {
            System.out.println("no agent");
            System.out.println("value-now=" + Callee.value());
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
        long[] p = parkedResult;
        System.out.println("parked after-old=" + p[0] + " after-new=" + (p[1] == AFTER - 1));
        long[] s = self(i);
        System.out.println("self after-old=" + s[0] + " after-new=" + (s[1] == AFTER - 1));
        System.out.println("value-now=" + Callee.value());
    }
}
