// Interpreter round i1 wave 48, lane L1: stage 1 of
// docs/known-issues/interpreter/i47-L1-proposal-compatible-runs-the-blocking-methods-bytecode-under-a-debugger-20261011.md.
//
// The debuggee starts two daemon threads only AFTER the debugger has attached
// (so no stand-in is already running when the debugger arrives): `sleeper`
// loops in `Thread.sleep(50)`, `waiter` in `M.wait(LONG)` on a monitor of
// class `L1W48JdiCompatibleBlockingBytecode$Mon`. The debugger suspends the
// VM once both block and prints their top frames; asks `ThisObject` of the
// waiter's `Object.wait` frame (a Java frame on HotSpot: it answers the
// monitor; an opaque row answers OPAQUE_FRAME, which JDI reports as an
// exception); asks `visibleVariables` of `Thread.sleep`'s frame (HotSpot
// answers its two, `millis` and `nanos`: Temurin's `java.base` carries a
// `LocalVariableTable`); then steps OUT of the sleeper's native top frame (a
// line step, no class filters) and prints where the step completes. (A
// STEP_OVER from that native never completed on HotSpot 25.0.3 within 60 s,
// and a STEP_INTO completed where STEP_OUT does; the probe asks STEP_OUT.)
//
// Canonical transcript: no ids, no addresses, no timings.
// Run by the conformance runner, tools/jdi/run-jdi-conformance.sh
// (`--scenario L1W48JdiCompatibleBlockingBytecode`).
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   ended
//
// Under a debugger:
//
//   javac -g -d out L1W48JdiCompatibleBlockingBytecode.java
//   cratonvm --java-home $JDK --compatible --jdwp-port 5791 -cp out L1W48JdiCompatibleBlockingBytecode wait
//   java -cp out L1W48JdiCompatibleBlockingBytecode debug 5791 > transcript.txt
//
// HotSpot 25.0.3 as the debuggee (the Windows box, two runs, the same each
// time; the JDK line numbers are 25.0.3's):
//
//   == attach
//   == suspended
//     sleeper frames: java.lang.Thread.sleepNanos0[native] java.lang.Thread.sleepNanos:509 java.lang.Thread.sleep:540 L1W48JdiCompatibleBlockingBytecode.lambda$main$0:99
//     waiter frames: java.lang.Object.wait0[native] java.lang.Object.wait:389 L1W48JdiCompatibleBlockingBytecode.lambda$main$1:108
//     waiter thisObject(1): wait -> L1W48JdiCompatibleBlockingBytecode$Mon
//     sleeper visibleVariables(2): sleep -> 2
//     step out of the native: java.lang.Thread.sleepNanos
//   == end
//     disconnected
//
// CratonVM `--compatible` before wave 48 (Windows `livedbg` build, measured):
// the stand-ins run, so the frames are listed as OPAQUE rows (wave 47; the
// two `frames` rows match) but
//     waiter thisObject(1): OpaqueFrameException
//     step out of the native: L1W48JdiCompatibleBlockingBytecode.lambda$main$0
// (the step's depth counts the native and not the opaque levels).
// `visibleVariables` matched: JDI reads it from the class, not the VM.
// `--jdk-only` already matched (it runs the bodies since wave 46). One
// `--compatible` run in three also failed with JDI's `frame(0)` throwing
// `IndexOutOfBoundsException` for a sleeper suspended between two sleeps
// (`Frames` answered nothing before the thread reached its suspend point;
// fixed in wave 48, `debug::await_suspend_point_frames`).
// With `CRATONVM_DEBUG_COMPATIBLE_BLOCKING_BYTECODE` on (the default since
// wave 48) `--compatible` runs the JDK bodies of `Thread.sleep` and
// `Object.wait` while the debugger is attached. Positive control:
// `CRATONVM_FRAME_TRACE=1` prints `[STOOD_IN_BLOCKING]
// java/lang/Thread.sleep(J)V` and `[STOOD_IN_BLOCKING]
// java/lang/Object.wait(J)V` under `--compatible`; with the switch off it
// prints neither there.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W48JdiCompatibleBlockingBytecode {
    /** Set by the debugger: 1 = start the threads, 2 = done. */
    static volatile int phase;
    static final long LONG = 600_000L;

    static final class Mon {
    }

    static final Mon M = new Mon();

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
        boolean debugged = args.length == 1 && args[0].equals("wait");
        long until = System.currentTimeMillis() + 300_000;
        while (debugged && phase == 0 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        Thread sleeper = daemon("sleeper", () -> {
            try {
                while (phase < 2) {
                    Thread.sleep(50);
                }
            } catch (InterruptedException stop) {
                // Ends with the program.
            }
        });
        Thread waiter = daemon("waiter", () -> {
            synchronized (M) {
                try {
                    M.wait(LONG);
                } catch (InterruptedException stop) {
                    // Ends with the program.
                }
            }
        });
        sleeper.start();
        waiter.start();
        while (debugged && phase < 2 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        waiter.interrupt();
        sleeper.interrupt();
        System.out.println("ended");
    }

    static final class Debugger {
        static VirtualMachine vm;

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
                if (m.declaringType().name().startsWith("L1W48JdiCompatibleBlockingBytecode")) {
                    break;
                }
            }
            return sb.toString();
        }

        static String thisObject(ThreadReference t, int index) {
            try {
                StackFrame f = t.frame(index);
                ObjectReference o = f.thisObject();
                return f.location().method().name() + " -> "
                        + (o == null ? "null" : o.referenceType().name());
            } catch (Exception e) {
                return e.getClass().getSimpleName();
            }
        }

        static String locals(ThreadReference t, int index) {
            try {
                StackFrame f = t.frame(index);
                return f.location().method().name() + " -> " + f.visibleVariables().size();
            } catch (Exception e) {
                return e.getClass().getSimpleName();
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

        static boolean blocked(String name) throws Exception {
            ThreadReference t = thread(name);
            if (t == null) {
                return false;
            }
            t.suspend();
            try {
                return t.frameCount() > 0 && t.frame(0).location().method().isNative();
            } finally {
                t.resume();
            }
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
            ClassType type = (ClassType) awaitClass("L1W48JdiCompatibleBlockingBytecode");
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(1));
            long until = System.currentTimeMillis() + 120_000;
            while (!(blocked("sleeper") && blocked("waiter"))) {
                if (System.currentTimeMillis() > until) {
                    fail("the threads never blocked");
                }
                Thread.sleep(50);
            }
            ThreadReference sleeper = thread("sleeper");
            ThreadReference waiter = thread("waiter");
            vm.suspend();
            // A sleeper caught between two sleeps: let it block again.
            while (!sleeper.frame(0).location().method().isNative()) {
                vm.resume();
                Thread.sleep(5);
                vm.suspend();
                if (System.currentTimeMillis() > until) {
                    fail("the sleeper never blocked again");
                }
            }
            System.out.println("== suspended");
            System.out.println("  sleeper frames: " + frames(sleeper));
            System.out.println("  waiter frames: " + frames(waiter));
            System.out.println("  waiter thisObject(1): " + thisObject(waiter, 1));
            System.out.println("  sleeper visibleVariables(2): " + locals(sleeper, 2));
            EventRequestManager erm = vm.eventRequestManager();
            StepRequest step = erm.createStepRequest(sleeper, StepRequest.STEP_LINE, StepRequest.STEP_OUT);
            step.addCountFilter(1);
            step.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            step.enable();
            vm.resume();
            String landed = null;
            while (landed == null) {
                EventSet set = vm.eventQueue().remove(60_000);
                if (set == null) {
                    fail("the step never completed");
                }
                for (Event e : set) {
                    if (e instanceof StepEvent) {
                        Location l = ((StepEvent) e).location();
                        landed = l.declaringType().name() + "." + l.method().name();
                    }
                }
                if (landed == null) {
                    set.resume();
                }
            }
            System.out.println("  step out of the native: " + landed);
            erm.deleteEventRequest(step);
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(2));
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
