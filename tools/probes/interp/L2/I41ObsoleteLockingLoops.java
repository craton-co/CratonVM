// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Shape 1 (b) of
// docs/internal/fixed-bugs/i41-L1-compiled-bodies-without-an-exit-finish-their-activation-compiled-FIXED-20261007.md:
// an agent's interpreter-only request (JVMTI SingleStep for one thread)
// reaches a compiled loop that is its class's OBSOLETE activation -- the
// class was redefined while the loop ran -- when the method is
// `synchronized` or the loop runs inside a `synchronized` block. HotSpot
// deoptimizes the thread's compiled frames once single stepping is on, so
// the obsolete activation is stepped, interpreted, in the bytecode it was
// running.
//
// Each row warms its loop (20000 short calls through reflection, so a VM door
// enters its method-entry body), starts it on a worker for
// good, redefines the loop's class (one per row) with its own bytes
// (java.lang.instrument), arms SingleStep for the worker, waits 300 ms, stops
// the loop and prints whether a step arrived in the loop's own method:
//   plain  an unsynchronized loop (fixed in wave 47; the control)
//   sync   a `static synchronized` method's loop
//   block  a loop inside `synchronized (LOCK)`
//   held   the loop's monitor is still the worker's while it is stepped
//          (the worker's own locked-monitor list)
//   released  main takes the monitor once the worker returned (a monitor the
//          resumed frame failed to release would hang the probe here)
//
// HotSpot 25.0.3 (Rust port of the shim on Windows, default flags and -Xint):
//   plain stepped=true
//   sync stepped=true held=true released=true
//   block stepped=true held=true released=true
//
// CratonVM before the shape-1 (b) work (debug build with
// `experimental-debug`, `--jdk-only`): `plain stepped=true`, `sync` and
// `block` `stepped=false` (the obsolete body stays compiled:
// `entry_resumable_as_obsolete` refused a synchronized or locking method).
//
// SETUP:
//   cargo build --release --manifest-path tools/probes/interp/L1/rust-ports/I41ObsoleteLockingLoops/Cargo.toml
//   (the shim: target/release/i41_obsolete_locking_loops.dll)
//   a jar whose manifest has
//     Premain-Class: I41ObsoleteLockingLoops$Agent
//     Can-Redefine-Classes: true
//   containing I41ObsoleteLockingLoops*.class, then
//     java|cratonvm -agentpath:<dll> -javaagent:probe.jar -cp probe.jar I41ObsoleteLockingLoops <dll> <plain|sync|block>
// (one row per process; without the row argument all three run in one
// process, which on CratonVM leaves the second and third loops interpreted)
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.Instrumentation;
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;

public class I41ObsoleteLockingLoops {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    static native int armStep(Thread t);

    static native int disarmStep(Thread t);

    static native String report();

    /** One class per row: a redefinition makes every body of its class obsolete. */
    static volatile boolean stop;
    static volatile boolean in;

    public static class Plain {
        static long spin(boolean forGood) {
            long n = 0;
            if (forGood) {
                in = true;
            }
            while (!stop) {
                n++;
                if (!forGood && n > 200) {
                    break;
                }
            }
            return n;
        }
    }

    public static class Sync {
        static synchronized long spin(boolean forGood) {
            long n = 0;
            if (forGood) {
                in = true;
            }
            while (!stop) {
                n++;
                if (!forGood && n > 200) {
                    break;
                }
            }
            return n;
        }
    }

    public static class Block {
        static final Object LOCK = new Object();

        static long spin(boolean forGood) {
            long n = 0;
            synchronized (LOCK) {
                if (forGood) {
                    in = true;
                }
                while (!stop) {
                    n++;
                    if (!forGood && n > 200) {
                        break;
                    }
                }
            }
            return n;
        }
    }

    static byte[] bytesOf(Class<?> c) throws Exception {
        String name = c.getName();
        try (InputStream in = I41ObsoleteLockingLoops.class.getResourceAsStream(
                name.substring(name.lastIndexOf('.') + 1) + ".class")) {
            return in.readAllBytes();
        }
    }

    /** Is `lock` held by `owner` (from the thread's own lock list)? */
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

    /**
     * Through reflection only, so a VM door (not a compiled caller) enters
     * the loop's METHOD-ENTRY body, warmed by the same door.
     */
    static long call(java.lang.reflect.Method spin, boolean forGood) {
        try {
            return (Long) spin.invoke(null, forGood);
        } catch (ReflectiveOperationException e) {
            throw new RuntimeException(e);
        }
    }

    static void row(String label, Class<?> target, Object lock) throws Exception {
        java.lang.reflect.Method spin = target.getDeclaredMethod("spin", boolean.class);
        spin.setAccessible(true);
        for (int i = 0; i < 20000; i++) {
            call(spin, false);
        }
        Thread.sleep(500);
        stop = false;
        in = false;
        Thread worker = new Thread(() -> call(spin, true), "spinner-" + label);
        worker.start();
        while (!in) {
            Thread.sleep(1);
        }
        Thread.sleep(100);
        inst.redefineClasses(new ClassDefinition(target, bytesOf(target)));
        int armed = armStep(worker);
        Thread.sleep(300);
        boolean held = lock != null && holds(worker, lock);
        stop = true;
        worker.join();
        disarmStep(worker);
        String steps = report();
        boolean stepped = (" " + steps).contains(" spin=");
        // A lock the resumed frame failed to release would block here for good.
        boolean released = false;
        if (lock != null) {
            synchronized (lock) {
                released = true;
            }
        }
        System.out.println(label + " stepped=" + stepped
                + (lock != null ? " held=" + held + " released=" + released : "")
                + (armed != 0 ? " arm-error=" + armed : ""));
    }

    public static void main(String[] args) throws Exception {
        if (args.length < 1 || inst == null) {
            System.out.println("usage: -agentpath:<shim> -javaagent:probe.jar I41ObsoleteLockingLoops <shim>");
            return;
        }
        System.load(args[0]);
        // One row per process when a second argument names it: on CratonVM a
        // SingleStep request leaves the JIT off for the rest of the run, so a
        // later row's loop would never be compiled.
        String only = args.length > 1 ? args[1] : "all";
        if (only.equals("all") || only.equals("plain")) {
            row("plain", Plain.class, null);
        }
        if (only.equals("all") || only.equals("sync")) {
            row("sync", Sync.class, Sync.class);
        }
        if (only.equals("all") || only.equals("block")) {
            row("block", Block.class, Block.LOCK);
        }
    }
}
