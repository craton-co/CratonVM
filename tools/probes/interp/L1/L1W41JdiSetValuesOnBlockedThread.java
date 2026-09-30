// Interpreter round i1 wave 41, lane L1: `StackFrame.SetValues` on a thread
// the debugger suspended while it was blocked in a native (`Object.wait0`),
// item 3 of
// docs/internal/fixed-bugs/interpreter-L1-jdwp-suspension-does-not-reach-compiled-or-native-code-FIXED-20261005.md.
//
// The debuggee's `main` waits on a monitor inside `blocked(5)`, whose locals
// are `x` (the parameter), `s`, `l` and `d`. The debugger suspends `main`
// while it waits (inside the native `Object.wait0`), writes all four
// locals of the `blocked` frame, reads them back, resumes and suspends the
// thread again (still waiting: the new listing must show the new values),
// resumes it, and only then lets a helper thread notify the monitor. `blocked` returns its
// locals as a string, which the debugger reads at a breakpoint in `finish`.
//
// Canonical transcript: no ids, no addresses, no timings.
// Run by the conformance runner, tools/jdi/run-jdi-conformance.sh
// (`--scenario L1W41JdiSetValuesOnBlockedThread`).
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   x=5 s=orig l=7 d=1.5
//
// Under a debugger (the debuggee waits up to five minutes):
//
//   javac -g -d out L1W41JdiSetValuesOnBlockedThread.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L1W41JdiSetValuesOnBlockedThread wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5791 -cp out L1W41JdiSetValuesOnBlockedThread wait)
//   java -cp out L1W41JdiSetValuesOnBlockedThread debug 5791 > transcript.txt     # HotSpot 25's java
//
// HotSpot 25.0.3 as the debuggee (the debugger's stdout; three runs, the
// same each time):
//
//   == attach
//   == suspended while blocked
//     status: WAIT suspended=true
//     blocked frame found: true
//   == set the blocked frame's locals
//     set x: ok
//     set s: ok
//     set l: ok
//     set d: ok
//   == read them back before the resume
//     x = 42
//     s = "changed"
//     l = 70000000000
//     d = 2.25
//   == resumed, suspended again while still blocked
//     status: WAIT suspended=true
//     x = 42
//     s = "changed"
//     l = 70000000000
//     d = 2.25
//   == resumed and released
//     result = "x=42 s=changed l=70000000000 d=2.25"
//   == end
//     disconnected
//
// `main` is already waiting when the debugger attaches (the debuggee does not
// wait for it), as when an IDE connects to a running server. The first host
// run of wave 41 printed `blocked frame found: false` / `FAILED: no frame of
// blocked` in all four modes: a thread's inspection window opened only when
// it blocked with a session attached, so a thread that blocked before the
// attach listed no frame at all until it woke. Since the wave-41 follow-up
// the window opens whenever the VM runs a JDWP server
// (`interpreter::open_blocked_inspection`).
//
// CratonVM before wave 41: each `set` answered `THREAD_NOT_SUSPENDED` (the
// write ran only on a thread parked at an interpreter suspend point, and
// this one is blocked in a native), which JDI reports as
// `set x: com.sun.jdi.InternalException` (JDWP error 13), and the result
// kept the old values. Since wave 41 the write is recorded against the
// published snapshot (answered at once by `GetValues`) and applied by the
// thread itself as it leaves the blocking region
// (`debug::inspect::defer_blocked_frame_write`,
// `debug::inspect::apply_deferred_frame_writes_if_any`). Positive control:
// `CRATONVM_FRAME_TRACE=1` prints `[DEFERRED_SETVALUES] recorded tid=<n>
// slots=1` once per `set` (JDI sends one `SetValues` each; four lines) and
// then `[DEFERRED_SETVALUES] applied tid=<n> writes=4 dropped=0` when the
// thread wakes. `--nojit` and `--compatible` must print the same transcript.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W41JdiSetValuesOnBlockedThread {
    /** 0 until the debugger lets the helper notify the monitor. */
    static volatile int go;
    /** 1 once `main` is about to wait. */
    static volatile int waiting;
    static final Object LOCK = new Object();
    static boolean released;
    static volatile String result = "none";

    static String blocked(int x) throws InterruptedException {
        String s = "orig";
        long l = 7L;
        double d = 1.5;
        synchronized (LOCK) {
            waiting = 1;
            while (!released) {
                LOCK.wait();
            }
        }
        return "x=" + x + " s=" + s + " l=" + l + " d=" + d;
    }

    static void finish() {
        // The debugger's breakpoint reads `result` here.
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean underDebugger = args.length == 1 && args[0].equals("wait");
        if (!underDebugger) {
            go = 1;
        }
        Thread helper = new Thread(() -> {
            long until = System.currentTimeMillis() + 300_000;
            while (go == 0 && System.currentTimeMillis() < until) {
                try {
                    Thread.sleep(10);
                } catch (InterruptedException e) {
                    return;
                }
            }
            // Only once `main` is inside `blocked`, so the plain run's notify
            // cannot come before its wait.
            while (waiting == 0) {
                Thread.onSpinWait();
            }
            synchronized (LOCK) {
                released = true;
                LOCK.notifyAll();
            }
        }, "helper");
        helper.setDaemon(true);
        helper.start();
        result = blocked(5);
        finish();
        System.out.println(result);
    }

    static final class Debugger {
        static VirtualMachine vm;

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
            EventRequestManager erm = vm.eventRequestManager();
            System.out.println("== attach");
            ClassType type = (ClassType) awaitClass("L1W41JdiSetValuesOnBlockedThread");
            ThreadReference main = null;
            for (ThreadReference t : vm.allThreads()) {
                if (t.name().equals("main")) {
                    main = t;
                }
            }
            if (main == null) {
                fail("no main thread");
            }
            Method finish = type.methodsByName("finish").get(0);
            BreakpointRequest bp = erm.createBreakpointRequest(finish.location());
            bp.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            bp.enable();

            System.out.println("== suspended while blocked");
            // Suspended while it waits: blocked in the wait's native, not
            // parked at a bytecode of `blocked` (where a write always worked).
            StackFrame frame = null;
            boolean waitingNow = false;
            long until = System.currentTimeMillis() + 120_000;
            while (System.currentTimeMillis() < until) {
                if (((IntegerValue) type.getValue(type.fieldByName("waiting"))).value() == 1
                        && main.status() == ThreadReference.THREAD_STATUS_WAIT) {
                    main.suspend();
                    if (main.status() == ThreadReference.THREAD_STATUS_WAIT) {
                        waitingNow = true;
                        break;
                    }
                    main.resume();
                }
                Thread.sleep(20);
            }
            if (!waitingNow) {
                fail("main never waited in blocked");
            }
            System.out.println("  status: WAIT suspended=" + main.isSuspended());
            for (StackFrame f : main.frames()) {
                if (f.location().method().name().equals("blocked")) {
                    frame = f;
                    break;
                }
            }
            System.out.println("  blocked frame found: " + (frame != null));
            if (frame == null) {
                fail("no frame of blocked");
            }

            System.out.println("== set the blocked frame's locals");
            set(frame, "x", vm.mirrorOf(42));
            set(frame, "s", vm.mirrorOf("changed"));
            set(frame, "l", vm.mirrorOf(70_000_000_000L));
            set(frame, "d", vm.mirrorOf(2.25));

            System.out.println("== read them back before the resume");
            for (StackFrame f : main.frames()) {
                if (f.location().method().name().equals("blocked")) {
                    frame = f;
                    break;
                }
            }
            for (String name : new String[] {"x", "s", "l", "d"}) {
                Value v = frame.getValue(frame.visibleVariableByName(name));
                System.out.println("  " + name + " = " + v);
            }

            // Still blocked: a resume and a new suspension list the frame
            // again, and the new listing shows the written values.
            System.out.println("== resumed, suspended again while still blocked");
            main.resume();
            Thread.sleep(200);
            main.suspend();
            System.out.println("  status: " + (main.status() == ThreadReference.THREAD_STATUS_WAIT
                    ? "WAIT" : "other") + " suspended=" + main.isSuspended());
            frame = null;
            for (StackFrame f : main.frames()) {
                if (f.location().method().name().equals("blocked")) {
                    frame = f;
                    break;
                }
            }
            if (frame == null) {
                fail("no frame of blocked after the second suspension");
            }
            for (String name : new String[] {"x", "s", "l", "d"}) {
                Value v = frame.getValue(frame.visibleVariableByName(name));
                System.out.println("  " + name + " = " + v);
            }

            System.out.println("== resumed and released");
            main.resume();
            type.setValue(type.fieldByName("go"), vm.mirrorOf(1));
            boolean ended = false;
            boolean hit = false;
            until = System.currentTimeMillis() + 120_000;
            while (!hit && !ended && System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(2_000);
                if (set == null) {
                    continue;
                }
                for (Event e : set) {
                    if (e instanceof BreakpointEvent) {
                        Value r = type.getValue(type.fieldByName("result"));
                        System.out.println("  result = " + r);
                        hit = true;
                        // While the thread is held: the VM ends soon after
                        // the resume.
                        erm.deleteEventRequest(bp);
                    } else if (e instanceof VMDeathEvent || e instanceof VMDisconnectEvent) {
                        ended = true;
                    }
                }
                if (!ended) {
                    set.resume();
                }
            }
            if (!hit) {
                System.out.println("  result: no breakpoint in finish");
            }

            System.out.println("== end");
            if (!ended && !hit) {
                erm.deleteEventRequest(bp);
            }
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

        static void set(StackFrame frame, String name, Value value) {
            try {
                frame.setValue(frame.visibleVariableByName(name), value);
                System.out.println("  set " + name + ": ok");
            } catch (Exception e) {
                System.out.println("  set " + name + ": " + e.getClass().getName());
            }
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
