// Interpreter round i1 wave 47, lane L1: a thread that blocked in
// `Thread.sleep`, `Object.wait`, `LockSupport.park*` or `Thread.join` BEFORE
// the debugger attached lists HotSpot's JDK frames between the native it
// blocks in and its caller
// (docs/internal/fixed-bugs/interpreter-L1-a-sleep-or-wait-stand-in-lists-only-its-native-leaf-FIXED-20261011.md).
//
// Seven daemon threads block for ten minutes before the debugger attaches:
// `sleeper` in `Thread.sleep(long)`, `waiter` in `Object.wait()`,
// `timedWaiter` in `Object.wait(long)`, `parker` in `LockSupport.park()`,
// `nanoParker` in `LockSupport.parkNanos(Object, long)`, `joiner` in
// `Thread.join()` (of a thread that never ends) and `durationSleeper` in
// `Thread.sleep(Duration)`. The debugger suspends the VM and prints, for
// each, its status and its frames from the top down to its own `run` method
// (`name:line`, `[native]` for a native method), then tries
// `ForceEarlyReturn` and `PopFrames` on the top frame (HotSpot:
// `NativeMethodException` for both), and resumes; the debuggee interrupts or
// unparks every thread and ends.
//
// Canonical transcript: no ids, no addresses, no timings.
// Run by the conformance runner, tools/jdi/run-jdi-conformance.sh
// (`--scenario L1W47JdiBlockedBeforeAttach`).
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   ended
//
// Under a debugger (the debuggee waits up to five minutes):
//
//   javac -g -d out L1W47JdiBlockedBeforeAttach.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L1W47JdiBlockedBeforeAttach wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5791 -cp out L1W47JdiBlockedBeforeAttach wait)
//   java -cp out L1W47JdiBlockedBeforeAttach debug 5791 > transcript.txt     # HotSpot 25's java
//
// HotSpot 25.0.3 as the debuggee (the Windows box; the orchestrator should
// take the host's HotSpot run as the reference, whose JDK lines may differ):
//
//   == attach
//   == suspended
//     sleeper status: SLEEPING
//     sleeper frames: java.lang.Thread.sleepNanos0[native] java.lang.Thread.sleepNanos:509 java.lang.Thread.sleep:540 L1W47JdiBlockedBeforeAttach.lambda$main$0:114
//     sleeper forceEarlyReturn: NativeMethodException
//     sleeper popFrames: NativeMethodException
//     waiter status: WAIT
//     waiter frames: java.lang.Object.wait0[native] java.lang.Object.wait:389 java.lang.Object.wait:351 L1W47JdiBlockedBeforeAttach.lambda$main$1:122
//     waiter forceEarlyReturn: NativeMethodException
//     waiter popFrames: NativeMethodException
//     timedWaiter status: WAIT
//     timedWaiter frames: java.lang.Object.wait0[native] java.lang.Object.wait:389 L1W47JdiBlockedBeforeAttach.lambda$main$2:131
//     timedWaiter forceEarlyReturn: NativeMethodException
//     timedWaiter popFrames: NativeMethodException
//     parker status: WAIT
//     parker frames: jdk.internal.misc.Unsafe.park[native] java.util.concurrent.locks.LockSupport.park:369 L1W47JdiBlockedBeforeAttach.lambda$main$3:138
//     parker forceEarlyReturn: NativeMethodException
//     parker popFrames: NativeMethodException
//     nanoParker status: WAIT
//     nanoParker frames: jdk.internal.misc.Unsafe.park[native] java.util.concurrent.locks.LockSupport.parkNanos:271 L1W47JdiBlockedBeforeAttach.lambda$main$4:141
//     nanoParker forceEarlyReturn: NativeMethodException
//     nanoParker popFrames: NativeMethodException
//     joiner status: WAIT
//     joiner frames: java.lang.Object.wait0[native] java.lang.Object.wait:389 java.lang.Thread.join:1887 java.lang.Thread.join:1963 L1W47JdiBlockedBeforeAttach.lambda$main$6:146
//     joiner forceEarlyReturn: NativeMethodException
//     joiner popFrames: NativeMethodException
//     durationSleeper status: SLEEPING
//     durationSleeper frames: java.lang.Thread.sleepNanos0[native] java.lang.Thread.sleepNanos:509 java.lang.Thread.sleep:601 L1W47JdiBlockedBeforeAttach.lambda$main$7:153
//     durationSleeper forceEarlyReturn: NativeMethodException
//     durationSleeper popFrames: NativeMethodException
//   == end
//     disconnected
//
// (Measured with `java` 25.0.3 on the Windows box; the lambda numbers are
// this file's own lines.)
//
// CratonVM before wave 47 (the wave-46 host run, and from the code): a
// thread still inside a registered native standing in for the JDK method
// (every such thread under `--compatible`; under `--jdk-only` one that
// blocked before the debugger attached, as here) listed the native leaf
// directly above its caller (`sleeper frames:
// java.lang.Thread.sleepNanos0[native] L1W47...lambda$main$0:114`), and a
// thread in `LockSupport.park*` listed its caller on top with no native at
// all (`forceEarlyReturn: ok`). Now the listing's head is the stand-in's
// chain read from the real class bytes (`interpreter::NativeHead`,
// `stackwalker::debugger_standin_frames`). Positive control:
// `CRATONVM_FRAME_TRACE=1` prints `[STANDIN_HEAD]
// java/lang/Thread.sleep(J)V levels=2` (and one line per other thread) when
// the listing is built; the base prints no such line.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.time.Duration;
import java.util.*;
import java.util.concurrent.locks.LockSupport;

public class L1W47JdiBlockedBeforeAttach {
    /** Set by the debugger once it has looked. */
    static volatile int done;
    static final Object W = new Object();
    static final Object TW = new Object();
    static final long LONG = 600_000L;

    static Thread daemon(String name, Runnable body) {
        Thread t = new Thread(body, name);
        t.setDaemon(true);
        return t;
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        Thread sleeper = daemon("sleeper", () -> {
            try {
                Thread.sleep(LONG);
            } catch (InterruptedException stop) {
                // Ends with the program.
            }
        });
        Thread waiter = daemon("waiter", () -> {
            synchronized (W) {
                try {
                    W.wait();
                } catch (InterruptedException stop) {
                    // Ends with the program.
                }
            }
        });
        Thread timedWaiter = daemon("timedWaiter", () -> {
            synchronized (TW) {
                try {
                    TW.wait(LONG);
                } catch (InterruptedException stop) {
                    // Ends with the program.
                }
            }
        });
        Thread parker = daemon("parker", () -> {
            LockSupport.park();
        });
        Thread nanoParker = daemon("nanoParker", () -> {
            LockSupport.parkNanos(W, LONG * 1_000_000L);
        });
        Thread never = daemon("never", () -> LockSupport.parkNanos(LONG * 1_000_000L));
        Thread joiner = daemon("joiner", () -> {
            try {
                never.join();
            } catch (InterruptedException stop) {
                // Ends with the program.
            }
        });
        Thread durationSleeper = daemon("durationSleeper", () -> {
            try {
                Thread.sleep(Duration.ofMinutes(10));
            } catch (InterruptedException stop) {
                // Ends with the program.
            }
        });
        Thread[] all = {sleeper, waiter, timedWaiter, parker, nanoParker, never, joiner, durationSleeper};
        for (Thread t : all) {
            t.start();
        }
        if (args.length == 1 && args[0].equals("wait")) {
            long until = System.currentTimeMillis() + 300_000;
            while (done == 0 && System.currentTimeMillis() < until) {
                Thread.sleep(10);
            }
        }
        for (Thread t : all) {
            t.interrupt();
            LockSupport.unpark(t);
        }
        System.out.println("ended");
    }

    static final class Debugger {
        static VirtualMachine vm;
        static final String[] NAMES = {
            "sleeper", "waiter", "timedWaiter", "parker", "nanoParker", "joiner", "durationSleeper"
        };

        static String frames(ThreadReference t) throws Exception {
            StringBuilder sb = new StringBuilder();
            for (StackFrame f : t.frames()) {
                if (sb.length() > 0) {
                    sb.append(' ');
                }
                Location l = f.location();
                Method m = l.method();
                sb.append(m.declaringType().name()).append('.').append(m.name());
                if (m.isNative()) {
                    sb.append("[native]");
                } else {
                    sb.append(':').append(l.lineNumber());
                }
                if (m.declaringType().name().equals("L1W47JdiBlockedBeforeAttach")) {
                    break;
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

        static ThreadReference thread(String name) {
            for (ThreadReference t : vm.allThreads()) {
                if (t.name().equals(name)) {
                    return t;
                }
            }
            return null;
        }

        /** Has every thread reached its blocking call (a non-RUNNING status)? */
        static boolean allBlocked() {
            for (String n : NAMES) {
                ThreadReference t = thread(n);
                if (t == null || t.status() == ThreadReference.THREAD_STATUS_RUNNING
                        || t.status() == ThreadReference.THREAD_STATUS_NOT_STARTED) {
                    return false;
                }
            }
            return true;
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
            ClassType type = (ClassType) awaitClass("L1W47JdiBlockedBeforeAttach");
            long until = System.currentTimeMillis() + 120_000;
            while (!allBlocked()) {
                if (System.currentTimeMillis() > until) {
                    fail("the threads never blocked");
                }
                Thread.sleep(50);
            }
            vm.suspend();
            System.out.println("== suspended");
            for (String n : NAMES) {
                ThreadReference t = thread(n);
                System.out.println("  " + n + " status: " + status(t));
                System.out.println("  " + n + " frames: " + frames(t));
                System.out.println("  " + n + " forceEarlyReturn: " + force(t));
                System.out.println("  " + n + " popFrames: " + pop(t));
            }
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
