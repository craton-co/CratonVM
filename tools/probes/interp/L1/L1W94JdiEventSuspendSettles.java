// Interpreter round i4 wave 94, lane L1 -- probe for
// `CRATONVM_DEBUG_SUSPEND_EVENT_SETTLE` (default ON, `=0` off; the stage-2
// remainder of
// docs/internal/fixed-bugs/interpreter-i48-L1-proposal-jdwp-suspension-completes-before-its-reply-DONE-20261224.md):
// a `SUSPEND_EVENT_THREAD` event set goes out once its event thread stopped,
// and a `ClassPrepare` set the server poll sends with `SUSPEND_ALL` once every
// thread stopped, as HotSpot's back end suspends before it writes a set.
//
// The debuggee runs a spinner thread that counts without end, then (once the
// debugger's requests stand) prepares `L1W94LateAll` and `L1W94LateOne`. The
// debugger asks for `ClassPrepare` of the first with `SUSPEND_ALL` (it reads
// the spinner's count twice, 100 ms apart, as soon as the event arrives: the
// count must not move) and of the second with `SUSPEND_EVENT_THREAD` (it reads
// the event thread's suspension, status and frames as soon as the event
// arrives: suspended, RUNNING -- JDWP's status of a thread stopped in Java --
// and a listing whose bottom frame is `main`).
//
// Plain run (no debugger): stdout must equal HotSpot 25's: `ended`.
//
// Under a debugger (debug build, `--features experimental-debug`):
//
//   javac -g -d out L1W94JdiEventSuspendSettles.java
//   cratonvm [--compatible] --java-home $JDK --jdwp-port 5895 -cp out L1W94JdiEventSuspendSettles wait
//   java -cp out L1W94JdiEventSuspendSettles debug 5895
//   (A/B: the debuggee as is, then with CRATONVM_DEBUG_SUSPEND_EVENT_SETTLE=0;
//    the transcript must be HotSpot's with the switch on, every run. Off, the
//    `one` line may read `frames false` or a status other than 1 on a loaded
//    host. Positive control: CRATONVM_FRAME_TRACE=1 prints a `[SUSPEND_SETTLED]`
//    line for the `one` set when the event thread was still on its way.
//    HotSpot as the debuggee:
//    java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5895 -cp out L1W94JdiEventSuspendSettles wait)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3 debuggee, MEASURED in wave 116 on Windows; derived from the source and
// JDWP's back end: ClassPrepare is posted on the thread that links the class,
// here `main` at the `new`; the status of a suspended thread in Java is
// RUNNING = 1)
// == attach
// == requests
//   all L1W94LateAll stable true
//   one L1W94LateOne thread main suspended true status 1 frames true bottom main
// == end
//   disconnected
// HOTSPOT_EXPECTED_END
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W94JdiEventSuspendSettles {
    /** Set by the debugger: 1 = the requests stand, 3 = done. */
    static volatile int phase;
    static volatile boolean ready;
    static volatile long count;
    static volatile long sink;

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean debugged = args.length == 1 && args[0].equals("wait");
        Thread spinner = new Thread(() -> {
            while (true) {
                count++;
            }
        }, "spinner");
        spinner.setDaemon(true);
        spinner.start();
        ready = true;
        long until = System.currentTimeMillis() + 300_000;
        while (debugged && phase != 1 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        sink += new L1W94LateAll().value();
        sink += new L1W94LateOne().value();
        while (debugged && phase != 3 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        System.out.println("ended");
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
            System.out.println("== attach");
            ClassType main = (ClassType) awaitClass("L1W94JdiEventSuspendSettles");
            long until = System.currentTimeMillis() + 120_000;
            while (!((BooleanValue) main.getValue(main.fieldByName("ready"))).value()) {
                if (System.currentTimeMillis() > until) {
                    fail("the debuggee never started its spinner");
                }
                Thread.sleep(20);
            }
            EventRequestManager erm = vm.eventRequestManager();
            ClassPrepareRequest all = erm.createClassPrepareRequest();
            all.addClassFilter("L1W94LateAll");
            all.setSuspendPolicy(EventRequest.SUSPEND_ALL);
            all.enable();
            ClassPrepareRequest one = erm.createClassPrepareRequest();
            one.addClassFilter("L1W94LateOne");
            one.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            one.enable();
            System.out.println("== requests");
            main.setValue(main.fieldByName("phase"), vm.mirrorOf(1));
            boolean done = false;
            while (!done) {
                EventSet set = vm.eventQueue().remove(120_000);
                if (set == null) {
                    fail("no ClassPrepare event");
                }
                for (Event e : set) {
                    if (e instanceof ClassPrepareEvent cp) {
                        String name = cp.referenceType().name();
                        ThreadReference t = cp.thread();
                        if (name.equals("L1W94LateAll")) {
                            long c1 = ((LongValue) main.getValue(main.fieldByName("count"))).value();
                            Thread.sleep(100);
                            long c2 = ((LongValue) main.getValue(main.fieldByName("count"))).value();
                            System.out.println("  all " + name + " stable " + (c1 == c2));
                        } else if (name.equals("L1W94LateOne")) {
                            boolean suspended = t.isSuspended();
                            int status = t.status();
                            List<StackFrame> frames = t.frames();
                            String bottom = frames.isEmpty()
                                    ? "none"
                                    : frames.get(frames.size() - 1).location().method().name();
                            System.out.println("  one " + name + " thread " + t.name()
                                    + " suspended " + suspended + " status " + status
                                    + " frames " + !frames.isEmpty() + " bottom " + bottom);
                            done = true;
                        }
                    } else if (e instanceof VMDisconnectEvent) {
                        fail("disconnected");
                    }
                }
                set.resume();
            }
            erm.deleteEventRequest(all);
            erm.deleteEventRequest(one);
            main.setValue(main.fieldByName("phase"), vm.mirrorOf(3));
            System.out.println("== end");
            boolean gone = false;
            while (!gone) {
                EventSet next;
                try {
                    next = vm.eventQueue().remove(60_000);
                } catch (VMDisconnectedException disconnected) {
                    break;
                }
                if (next == null) {
                    fail("the debuggee did not end");
                }
                for (Event e : next) {
                    if (e instanceof VMDisconnectEvent) {
                        gone = true;
                    }
                }
                if (!gone) {
                    try {
                        next.resume();
                    } catch (VMDisconnectedException disconnected) {
                        break;
                    }
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

/** Prepared once the requests stand; its `ClassPrepare` suspends every thread. */
class L1W94LateAll {
    int value() {
        return 1;
    }
}

/** Prepared next; its `ClassPrepare` suspends the event thread only. */
class L1W94LateOne {
    int value() {
        return 2;
    }
}
