// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Shape 2 (c) of
// docs/internal/fixed-bugs/i41-L1-compiled-bodies-without-an-exit-finish-their-activation-compiled-FIXED-20261007.md:
// the activation of a `synchronized`, trap-free, call-free loop that a
// COMPILED caller made through a caller-held synchronized CALL (the caller
// takes the monitor and calls the body directly). Wave 54 let the
// activations a VM door entered leave (`L2W54BoundSyncLeafLeavesFromADoor`);
// this one finished compiled, because the caller's cold side treated every
// sentinel as an exception. HotSpot deoptimizes both frames once single
// stepping is on, so the loop is stepped, interpreted, holding its monitor.
//
// `spin` is pure arithmetic over a chunk of 10M iterations (a body with any
// splice, field read, untaken branch or call is not one the caller-held
// binding takes). `viaCaller` calls it directly, chunk after chunk until
// `stop`, and is compiled after it, so its compile binds it caller-held; the
// measured call is `viaCaller` from the worker. 300 ms in, an agent arms
// JVMTI SingleStep for the worker, and disarms it 300 ms later; then the
// loop is stopped. Rows:
//   stepped   the activation of `spin` that was running when stepping was
//             armed was stepped: the first step in `spin` lands inside its
//             loop (a later chunk's fresh activation starts at bci 0)
//   held      the worker held `spin`'s monitor meanwhile
//   released  main takes the monitor once the worker returned (a lost
//             release would hang here; a double one throws
//             IllegalMonitorStateException in the worker: `threw=...`)
//
// HotSpot 25.0.3 (Rust port of the shim on Windows; default flags and -Xint):
//   static stepped=true held=true released=true
//   flat stepped=true held=true released=true
//   instance stepped=true held=true released=true
//
// CratonVM (debug build with `experimental-debug`, measured): the
// caller-held route binds only a WRAPPED synchronized body, so the `static`
// and `flat` rows reach it under `CRATONVM_JIT_SELF_LOCKING_SYNC_STATIC=0`
// (a static synchronized method otherwise gets a self-locking body, whose
// exits hand their monitor over and which leaves already); `flat` reaches
// the SINGLE-PASS site under `CRATONVM_C2_SUPERSEDE=0` too (its optimizing
// body otherwise supersedes it). With
// `CRATONVM_JIT_CALLER_HELD_CALLEE_RESUME=0` (the code before shape 2 (c)):
//   static stepped=false held=true released=true
//   flat stepped=false held=true released=true
// with it on (default): HotSpot's lines, one `helper precise-resume of
// trapped callee ...spin` line each (`CRATONVM_DBG_DEOPT=1`). The `instance`
// row and every row under default flags, `--compatible` and `--nojit`:
// HotSpot's lines either way.
//
// Positive control: CRATONVM_DBG_JITC=1 prints `sync-direct binds ...spin...:
// a named-only poll-exit body, marked caller-held` and an
// `ir-sync-direct-call` / `sp-sync-direct-call` line naming `spin`.
//
// SETUP: the shim of tools/probes/interp/L1/rust-ports/I41ObsoleteLockingLoops
// (`cargo build --release`), a jar whose manifest has
//     Premain-Class: I41CallerHeldSyncLeafSteps$Agent
// containing I41CallerHeldSyncLeafSteps*.class, then (one row per process:
// on CratonVM a SingleStep request leaves the JIT off for the rest of the run)
//     java|cratonvm -agentpath:<dll> -javaagent:probe.jar -cp probe.jar I41CallerHeldSyncLeafSteps <dll> <static|flat|instance>
import java.lang.instrument.Instrumentation;
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;

public class I41CallerHeldSyncLeafSteps {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    static native int armStep(Thread t);

    static native int disarmStep(Thread t);

    static native String report();

    /** One chunk: about 10 ms compiled, a few seconds interpreted. */
    static final long CHUNK = 10_000_000L;
    static volatile boolean stop;
    static volatile boolean in;
    static volatile String threw;

    /**
     * Pure arithmetic: no field, no call, no untaken branch (an uncommon trap,
     * a static read's helper or a call each disqualify the body the
     * caller-held binding takes), so its optimizing body polls on the
     * named-body bit alone.
     */
    static synchronized long spin(long limit) {
        long n = 0;
        long h = 1;
        while (n < limit) {
            h = h * 0x9E3779B97F4A7C15L + n;
            n++;
        }
        return n + (h & 1);
    }

    /**
     * The compiled caller: its compile binds `spin` for a caller-held CALL.
     * The measured call loops over chunks until `stop`; each chunk is one
     * caller-held activation of `spin`.
     */
    static long viaCaller(long limit) {
        long total = 0;
        do {
            in = true;
            total += spin(limit);
        } while (limit == CHUNK && !stop);
        return total;
    }

    static final Object ASIDE = new Object();

    /**
     * A caller the optimizing tier does not build (the `synchronized` block
     * it never enters: CratonVM's optimizing builder refuses a method with
     * monitor bytecode), so this row exercises the SINGLE-PASS caller-held
     * site; `viaCaller` gets an optimizing body.
     */
    static long flat(long limit) {
        if (limit < 0) {
            synchronized (ASIDE) {
                in = false;
            }
        }
        in = true;
        return spin(limit);
    }

    static final class Inst {
        synchronized long spin(long limit) {
            long n = 0;
            long h = 1;
            while (n < limit) {
                h = h * 0x9E3779B97F4A7C15L + n;
                n++;
            }
            return n + (h & 1);
        }

        long viaCaller(long limit) {
            long total = 0;
            do {
                in = true;
                total += spin(limit);
            } while (limit == CHUNK && !stop);
            return total;
        }
    }

    static boolean holds(Thread owner, Object lock) {
        ThreadInfo info = ManagementFactory.getThreadMXBean()
                .getThreadInfo(new long[] {owner.threadId()}, true, false)[0];
        if (info == null) {
            return false;
        }
        int id = System.identityHashCode(lock);
        for (var m : info.getLockedMonitors()) {
            if (m.getIdentityHashCode() == id) {
                return true;
            }
        }
        return false;
    }

    interface Call {
        long call(long limit);
    }

    static void row(String label, Call spinAlone, Call viaCaller, Object lock) throws Exception {
        // The callee alone first, so its body is published before the caller
        // compiles and binds it; then the caller.
        long w = 0;
        for (int k = 0; k < 20_000; k++) {
            w += spinAlone.call(200);
        }
        Thread.sleep(500);
        for (int k = 0; k < 20_000; k++) {
            w += viaCaller.call(200);
        }
        Thread.sleep(500);
        stop = false;
        in = false;
        threw = null;
        Thread worker = new Thread(() -> {
            try {
                viaCaller.call(CHUNK);
            } catch (Throwable t) {
                threw = t.toString();
            }
        }, "spinner-" + label);
        worker.start();
        while (!in) {
            Thread.sleep(1);
        }
        Thread.sleep(300);
        int armed = armStep(worker);
        boolean held = false;
        for (int k = 0; k < 30; k++) {
            Thread.sleep(10);
            held |= holds(worker, lock);
        }
        // Disarmed before the loop is stopped: a chunk resumed in the
        // interpreter then finishes at interpreter speed, not one step event
        // per bytecode.
        disarmStep(worker);
        stop = true;
        worker.join();
        String steps = report();
        // `spin=<count>@<first location>`: the first step in `spin` lands
        // inside the loop when the RUNNING activation was stepped (it left at
        // its back-edge poll), and at bci 0 when only the next chunk's new
        // activation ran interpreted.
        long first = -1;
        for (String item : steps.trim().split(" ")) {
            if (item.startsWith("spin=")) {
                first = Long.parseLong(item.substring(item.indexOf('@') + 1));
            }
        }
        boolean stepped = first > 0;
        boolean released = false;
        synchronized (lock) {
            released = true;
        }
        System.out.println(label + " stepped=" + stepped + " held=" + held + " released=" + released
                + (threw != null ? " threw=" + threw : "") + (armed != 0 ? " arm-error=" + armed : "")
                + (w < 0 ? " warm=" + w : ""));
    }

    public static void main(String[] args) throws Exception {
        if (args.length < 1 || inst == null) {
            System.out.println("usage: -agentpath:<shim> -javaagent:probe.jar I41CallerHeldSyncLeafSteps <shim> [static|flat|instance]");
            return;
        }
        System.load(args[0]);
        String only = args.length > 1 ? args[1] : "static";
        if (only.equals("flat")) {
            row("flat", I41CallerHeldSyncLeafSteps::spin, limit -> {
                long total = 0;
                do {
                    total += flat(limit);
                } while (limit == CHUNK && !stop);
                return total;
            }, I41CallerHeldSyncLeafSteps.class);
        } else if (only.equals("static")) {
            row("static", I41CallerHeldSyncLeafSteps::spin, I41CallerHeldSyncLeafSteps::viaCaller,
                    I41CallerHeldSyncLeafSteps.class);
        } else {
            Inst o = new Inst();
            row("instance", o::spin, o::viaCaller, o);
        }
    }
}
