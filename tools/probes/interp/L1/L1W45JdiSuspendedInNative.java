// Interpreter round i1 wave 45, lane L1: a thread suspended while it runs a
// native method stays in that native until it is resumed
// (docs/internal/fixed-bugs/interpreter-L1-a-thread-suspended-inside-a-native-method-parks-past-it-FIXED-20261009.md).
//
// Three daemon threads loop in short timed blocking natives: `sleeper` in
// `Thread.sleep(50)` (`Thread.sleepNanos0`), `waiter` in `W.wait(50)`
// (`Object.wait0`), `parker` in `LockSupport.parkNanos` (`Unsafe.park`). The
// debugger suspends the whole VM, waits 400 ms, long enough for every such
// native to have returned, and prints each thread's status and top three
// frames, then tries `ForceEarlyReturn` and `PopFrames` on its top frame.
// HotSpot holds a thread suspended in native code at its transition back to
// Java, so the native is still on top, and both commands answer OPAQUE_FRAME
// (JDI: `NativeMethodException`). HotSpot keeps `waiter` in `WAIT`; the
// other two read `RUNNING` there.
//
// Canonical transcript: no ids, no addresses, no timings.
// Run by the conformance runner, tools/jdi/run-jdi-conformance.sh
// (`--scenario L1W45JdiSuspendedInNative`).
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   ended
//
// Under a debugger (the debuggee waits up to five minutes):
//
//   javac -g -d out L1W45JdiSuspendedInNative.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L1W45JdiSuspendedInNative wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5791 -cp out L1W45JdiSuspendedInNative wait)
//   java -cp out L1W45JdiSuspendedInNative debug 5791 > transcript.txt     # HotSpot 25's java
//
// HotSpot 25.0.3 as the debuggee (four runs, the Windows box, the same each time):
//
//   == attach
//   == suspended
//     sleeper status: RUNNING suspended=true
//     sleeper top: java.lang.Thread.sleepNanos0[native] java.lang.Thread.sleepNanos java.lang.Thread.sleep
//     sleeper forceEarlyReturn: NativeMethodException
//     sleeper popFrames: NativeMethodException
//     sleeper top again: java.lang.Thread.sleepNanos0[native] java.lang.Thread.sleepNanos java.lang.Thread.sleep
//     waiter status: WAIT suspended=true
//     waiter top: java.lang.Object.wait0[native] java.lang.Object.wait L1W45JdiSuspendedInNative$Waiter.run
//     waiter forceEarlyReturn: NativeMethodException
//     waiter popFrames: NativeMethodException
//     waiter top again: java.lang.Object.wait0[native] java.lang.Object.wait L1W45JdiSuspendedInNative$Waiter.run
//     parker status: RUNNING suspended=true
//     parker top: jdk.internal.misc.Unsafe.park[native] java.util.concurrent.locks.LockSupport.parkNanos L1W45JdiSuspendedInNative$Parker.run
//     parker forceEarlyReturn: NativeMethodException
//     parker popFrames: NativeMethodException
//     parker top again: jdk.internal.misc.Unsafe.park[native] java.util.concurrent.locks.LockSupport.parkNanos L1W45JdiSuspendedInNative$Parker.run
//   == end
//     disconnected
//
// CratonVM before wave 45 (from the code, and the wave-43 host run of
// L1W43RawJdwpObjectErrorAnswers): each thread's native returned while it
// was suspended, and the thread ran on to its caller's next interpreter
// suspend point, so each `top` row begins with the caller
// (`java.lang.Thread.sleepNanos`, `java.lang.Object.wait`,
// `java.util.concurrent.locks.LockSupport.parkNanos`) and no native, the
// `waiter` read `RUNNING`, and `ForceEarlyReturn` and `PopFrames` were
// served on that caller (`ok` / `popped`, after which the `top again` rows
// differ too). Since wave 45 the thread parks at the native's return with
// the native on top (`interpreter::park_if_suspended_at_native_exit`).
// Positive control: `CRATONVM_FRAME_TRACE=1` prints
// `[NATIVE_EXIT_PARK] tid=<n> class=<id> method_id=<hash> depth=<frames>`
// once per thread (three lines, plus one for `main` if its 10 ms sleep
// returned while suspended); the base prints no such line. An
// `[NATIVE_EXIT_PARK] unnamed` line means the native could not be named
// and the thread parked at its caller, as on the base.
//
// `--compatible` may list `Thread.sleep` / `Object.wait` themselves as the
// native, where a registered native stands in for the JDK method.
//
// Wave 46 follow-up (lane L1): the host run of the merged wave-46 head
// listed `sleeper top: L1W45JdiSuspendedInNative$Sleeper.run` in all four
// modes (`forceEarlyReturn: ok`): `Thread.sleep` is served by a registered
// stand-in with no frame of its own. Now, while a debugger is attached,
// `--jdk-only` runs `Thread.sleep`'s bytecode instead
// (`jvmti_events::run_blocking_standin`; `CRATONVM_FRAME_TRACE=1` prints
// `[STOOD_IN_BLOCKING] java/lang/Thread.sleep(J)V`), which gives HotSpot's
// rows; `--compatible` keeps its stand-in and lists its native leaf,
// `sleeper top: java.lang.Thread.sleepNanos0[native]
// L1W45JdiSuspendedInNative$Sleeper.run` (predicted; allowed in
// tools/jdi/known-differences.txt), with HotSpot's `NativeMethodException`
// rows (docs/known-issues/interpreter/i46-L1-a-sleep-or-wait-stand-in-lists-only-its-native-leaf-20261010.md).
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.util.*;

public class L1W45JdiSuspendedInNative {
    /** Set by the debugger once it has looked. */
    static volatile int done;
    /** How many of the two looping threads have started looping. */
    static volatile int started;
    static final Object W = new Object();

    static final class Sleeper extends Thread {
        Sleeper() {
            super("sleeper");
            setDaemon(true);
        }

        @Override
        public void run() {
            synchronized (L1W45JdiSuspendedInNative.class) {
                started++;
            }
            try {
                while (true) {
                    Thread.sleep(50);
                }
            } catch (InterruptedException stop) {
                // Ends with the program.
            }
        }
    }

    static final class Waiter extends Thread {
        Waiter() {
            super("waiter");
            setDaemon(true);
        }

        @Override
        public void run() {
            synchronized (L1W45JdiSuspendedInNative.class) {
                started++;
            }
            try {
                while (true) {
                    synchronized (W) {
                        W.wait(50);
                    }
                }
            } catch (InterruptedException stop) {
                // Ends with the program.
            }
        }
    }

    static final class Parker extends Thread {
        Parker() {
            super("parker");
            setDaemon(true);
        }

        @Override
        public void run() {
            synchronized (L1W45JdiSuspendedInNative.class) {
                started++;
            }
            while (true) {
                java.util.concurrent.locks.LockSupport.parkNanos(50_000_000L);
            }
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        new Sleeper().start();
        new Waiter().start();
        new Parker().start();
        if (args.length == 1 && args[0].equals("wait")) {
            long until = System.currentTimeMillis() + 300_000;
            while (done == 0 && System.currentTimeMillis() < until) {
                Thread.sleep(10);
            }
        }
        System.out.println("ended");
    }

    static final class Debugger {
        static VirtualMachine vm;

        static String top(ThreadReference t) throws Exception {
            StringBuilder sb = new StringBuilder();
            List<StackFrame> frames = t.frames();
            for (int i = 0; i < 3 && i < frames.size(); i++) {
                if (sb.length() > 0) {
                    sb.append(' ');
                }
                Method m = frames.get(i).location().method();
                sb.append(m.declaringType().name()).append('.').append(m.name());
                if (m.isNative()) {
                    sb.append("[native]");
                }
            }
            return sb.toString();
        }

        static String force(ThreadReference t) {
            try {
                t.forceEarlyReturn(vm.mirrorOfVoid());
                return "ok";
            } catch (Exception e) {
                return e.getClass().getSimpleName();
            }
        }

        static String pop(ThreadReference t) {
            try {
                t.popFrames(t.frame(0));
                return "popped";
            } catch (Exception e) {
                return e.getClass().getSimpleName();
            }
        }

        static String status(ThreadReference t) {
            switch (t.status()) {
                case ThreadReference.THREAD_STATUS_ZOMBIE: return "ZOMBIE";
                case ThreadReference.THREAD_STATUS_RUNNING: return "RUNNING";
                case ThreadReference.THREAD_STATUS_SLEEPING: return "SLEEPING";
                case ThreadReference.THREAD_STATUS_MONITOR: return "MONITOR";
                case ThreadReference.THREAD_STATUS_WAIT: return "WAIT";
                case ThreadReference.THREAD_STATUS_NOT_STARTED: return "NOT_STARTED";
                default: return "UNKNOWN(" + t.status() + ")";
            }
        }

        static void look(String name, ThreadReference t) throws Exception {
            System.out.println("  " + name + " status: " + status(t) + " suspended=" + t.isSuspended());
            System.out.println("  " + name + " top: " + top(t));
            System.out.println("  " + name + " forceEarlyReturn: " + force(t));
            System.out.println("  " + name + " popFrames: " + pop(t));
            System.out.println("  " + name + " top again: " + top(t));
        }

        static void run(int port) throws Exception {
            AttachingConnector socket = null;
            for (AttachingConnector c : Bootstrap.virtualMachineManager().attachingConnectors()) {
                if (c.name().equals("com.sun.jdi.SocketAttach")) {
                    socket = c;
                }
            }
            Map<String, Connector.Argument> a = socket.defaultArguments();
            a.get("hostname").setValue("localhost");
            a.get("port").setValue(Integer.toString(port));
            a.get("timeout").setValue("60000");
            vm = attach(socket, a);
            System.out.println("== attach");
            ClassType type = (ClassType) awaitClass("L1W45JdiSuspendedInNative");
            long until = System.currentTimeMillis() + 120_000;
            while (((IntegerValue) type.getValue(type.fieldByName("started"))).value() < 3) {
                if (System.currentTimeMillis() > until) {
                    fail("the threads never started");
                }
                Thread.sleep(50);
            }
            // Let both loops run a few rounds.
            Thread.sleep(200);
            vm.suspend();
            // Every 50 ms native has returned by now, on a VM that lets it.
            Thread.sleep(400);
            ThreadReference sleeper = null;
            ThreadReference waiter = null;
            ThreadReference parker = null;
            for (ThreadReference t : vm.allThreads()) {
                if (t.name().equals("sleeper")) {
                    sleeper = t;
                } else if (t.name().equals("waiter")) {
                    waiter = t;
                } else if (t.name().equals("parker")) {
                    parker = t;
                }
            }
            if (sleeper == null || waiter == null || parker == null) {
                fail("no sleeper, waiter or parker thread");
            }
            System.out.println("== suspended");
            look("sleeper", sleeper);
            look("waiter", waiter);
            look("parker", parker);
            type.setValue(type.fieldByName("done"), vm.mirrorOf(1));
            vm.resume();
            System.out.println("== end");
            boolean ended = false;
            while (!ended) {
                EventSet set;
                try {
                    set = vm.eventQueue().remove(60_000);
                } catch (VMDisconnectedException gone) {
                    break;
                }
                if (set == null) {
                    fail("the debuggee did not end");
                }
                for (Event e : set) {
                    if (e instanceof VMDisconnectEvent) {
                        ended = true;
                    }
                }
                if (!ended) {
                    set.resume();
                }
            }
            System.out.println("  disconnected");
        }

        static ReferenceType awaitClass(String name) throws Exception {
            long until = System.currentTimeMillis() + 120_000;
            while (true) {
                List<ReferenceType> found = vm.classesByName(name);
                if (!found.isEmpty() && found.get(0).isPrepared()) {
                    return found.get(0);
                }
                if (System.currentTimeMillis() > until) {
                    fail("class " + name + " never loaded");
                }
                Thread.sleep(50);
            }
        }

        static VirtualMachine attach(AttachingConnector socket, Map<String, Connector.Argument> a)
                throws Exception {
            long until = System.currentTimeMillis() + 120_000;
            while (true) {
                try {
                    return socket.attach(a);
                } catch (java.io.IOException notYet) {
                    if (System.currentTimeMillis() > until) {
                        throw notYet;
                    }
                    Thread.sleep(250);
                }
            }
        }

        static void fail(String why) {
            System.out.println("FAILED: " + why);
            try {
                vm.dispose();
            } catch (RuntimeException ignored) {
                // Already gone.
            }
            System.exit(2);
        }
    }
}
