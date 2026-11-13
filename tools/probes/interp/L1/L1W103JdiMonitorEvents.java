// Interpreter round i5 wave 103, lane L1: the JDWP monitor events
// (`canRequestMonitorEvents`): `MonitorContendedEnter` / `Entered` and
// `MonitorWait` / `Waited`, through JDI, filtered to one lock with
// `addInstanceFilter` (the JDK's own monitors stay out of the transcript).
//
// HotSpot's back end reports them from the JVMTI monitor events
// (`eventHandler.c`, `cbMonitor*`). CratonVM answered the capability false
// and posted none; since wave 103 the monitor paths that post the C JVMTI
// events report them to JDWP too (vm/src/debug/mod.rs
// `post_jdwp_monitor_event`, from `jvmti::native_env::post_monitor_event`).
// Not built or run.
//
// `holder` takes the lock and keeps it for 500 ms; `main` enters it once
// `holder` has it (contended: enter, then entered), then waits on it for 50
// ms (wait, then waited, timed out); every request is SUSPEND_NONE.
//
// Canonical transcript: no ids, no addresses, no timings, no locations.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   ended
//
// Under a debugger (tools/jdi/run-jdi-conformance.sh runs it):
//
//   javac -g -d out L1W103JdiMonitorEvents.java
//   cratonvm --java-home $JDK --jdwp-port 5797 -cp out L1W103JdiMonitorEvents wait
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5797 \
//       -cp out L1W103JdiMonitorEvents wait
//   java -cp out L1W103JdiMonitorEvents debug 5797 > transcript.txt
//
// HotSpot 25 as the debuggee (derived from eventHandler.c, NOT measured):
//
//   == attach
//     canRequestMonitorEvents=true
//   == events
//     MonitorContendedEnterEvent thread=main monitor=L1W103JdiMonitorEvents$Lock
//     MonitorContendedEnteredEvent thread=main monitor=L1W103JdiMonitorEvents$Lock
//     MonitorWaitEvent thread=main monitor=L1W103JdiMonitorEvents$Lock timeout=50
//     MonitorWaitedEvent thread=main monitor=L1W103JdiMonitorEvents$Lock timedout=true
//   == end
//     disconnected
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W103JdiMonitorEvents {
    static final class Lock {
    }

    /** Set by the debugger: 1 = start, 3 = done. 2 is the debuggee's ack. */
    static volatile int phase;
    static final Lock lock = new Lock();
    static volatile boolean held;

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
        Thread holder = new Thread(() -> {
            synchronized (lock) {
                held = true;
                try {
                    Thread.sleep(500);
                } catch (InterruptedException ignored) {
                    // Ends the hold.
                }
            }
        }, "holder");
        holder.start();
        while (!held) {
            Thread.sleep(1);
        }
        synchronized (lock) {
            lock.wait(50);
        }
        holder.join();
        if (debugged) {
            phase = 2;
        }
        while (debugged && phase != 3 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        System.out.println("ended");
    }

    static final class Debugger {
        static VirtualMachine vm;
        static ClassType type;

        static String describe(Event e) {
            if (e instanceof MonitorContendedEnterEvent m) {
                return "MonitorContendedEnterEvent thread=" + m.thread().name()
                        + " monitor=" + m.monitor().referenceType().name();
            }
            if (e instanceof MonitorContendedEnteredEvent m) {
                return "MonitorContendedEnteredEvent thread=" + m.thread().name()
                        + " monitor=" + m.monitor().referenceType().name();
            }
            if (e instanceof MonitorWaitEvent m) {
                return "MonitorWaitEvent thread=" + m.thread().name()
                        + " monitor=" + m.monitor().referenceType().name()
                        + " timeout=" + m.timeout();
            }
            if (e instanceof MonitorWaitedEvent m) {
                return "MonitorWaitedEvent thread=" + m.thread().name()
                        + " monitor=" + m.monitor().referenceType().name()
                        + " timedout=" + m.timedout();
            }
            return null;
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
            System.out.println("  canRequestMonitorEvents=" + vm.canRequestMonitorEvents());
            type = (ClassType) awaitClass("L1W103JdiMonitorEvents");
            ObjectReference monitor = (ObjectReference) type.getValue(type.fieldByName("lock"));
            List<EventRequest> requests = new ArrayList<>();
            if (vm.canRequestMonitorEvents()) {
                EventRequestManager erm = vm.eventRequestManager();
                MonitorContendedEnterRequest enter = erm.createMonitorContendedEnterRequest();
                enter.addInstanceFilter(monitor);
                requests.add(enter);
                MonitorContendedEnteredRequest entered = erm.createMonitorContendedEnteredRequest();
                entered.addInstanceFilter(monitor);
                requests.add(entered);
                MonitorWaitRequest waitReq = erm.createMonitorWaitRequest();
                waitReq.addInstanceFilter(monitor);
                requests.add(waitReq);
                MonitorWaitedRequest waited = erm.createMonitorWaitedRequest();
                waited.addInstanceFilter(monitor);
                requests.add(waited);
                for (EventRequest r : requests) {
                    r.setSuspendPolicy(EventRequest.SUSPEND_NONE);
                    r.enable();
                }
            }
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(1));
            long until = System.currentTimeMillis() + 120_000;
            while (((IntegerValue) type.getValue(type.fieldByName("phase"))).value() != 2) {
                if (System.currentTimeMillis() > until) {
                    fail("phase 2 never came");
                }
                Thread.sleep(20);
            }
            System.out.println("== events");
            while (true) {
                EventSet next = vm.eventQueue().remove(1_000);
                if (next == null) {
                    break;
                }
                for (Event e : next) {
                    String row = describe(e);
                    if (row != null) {
                        System.out.println("  " + row);
                    }
                }
                next.resume();
            }
            for (EventRequest r : requests) {
                r.disable();
            }
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(3));
            System.out.println("== end");
            boolean ended = false;
            while (!ended) {
                EventSet next;
                try {
                    next = vm.eventQueue().remove(60_000);
                } catch (VMDisconnectedException gone) {
                    break;
                }
                if (next == null) {
                    fail("the debuggee did not end");
                }
                for (Event e : next) {
                    if (e instanceof VMDisconnectEvent) {
                        ended = true;
                    }
                }
                if (!ended) {
                    next.resume();
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
