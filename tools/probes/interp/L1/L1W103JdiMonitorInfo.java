// Interpreter round i5 wave 103, lane L1: JDWP `ObjectReference.MonitorInfo`
// (`canGetMonitorInfo`), through JDI's `owningThread()`, `entryCount()` and
// `waitingThreads()`.
//
// HotSpot's back end answers it from JVMTI `GetObjectMonitorUsage`: the
// owner, its entry count, and (JDK 23+) the threads waiting for a notify in
// `Object.wait` (`notify_waiters`). CratonVM answered `canGetMonitorInfo`
// false and served no `MonitorInfo`; since wave 103 it does
// (vm/src/debug/inspect.rs `or_monitor_info`). Not built or run.
//
// Canonical transcript: no ids, no addresses, no timings.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   ended
//
// Under a debugger (tools/jdi/run-jdi-conformance.sh runs it):
//
//   javac -g -d out L1W103JdiMonitorInfo.java
//   cratonvm --java-home $JDK --jdwp-port 5795 -cp out L1W103JdiMonitorInfo wait
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5795 \
//       -cp out L1W103JdiMonitorInfo wait
//   java -cp out L1W103JdiMonitorInfo debug 5795 > transcript.txt
//
// HotSpot 25 as the debuggee (derived from ObjectReferenceImpl.c and the
// JVMTI specification, NOT measured):
//
//   == attach
//     canGetMonitorInfo=true
//   == held twice, two waiters
//     owner=owner entryCount=2 waiting=w-1,w-2
//   == free
//     owner=null entryCount=0 waiting=w-1,w-2
//   == end
//     disconnected
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.util.*;

public class L1W103JdiMonitorInfo {
    /** Set by the debugger: 1 = start, 3 = release, 5 = done. 2 and 4 are the debuggee's acks. */
    static volatile int phase;
    static final Object lock = new Object();
    static volatile boolean release;

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
        List<Thread> waiters = new ArrayList<>();
        for (int i = 1; i <= 2; i++) {
            Thread w = new Thread(() -> {
                synchronized (lock) {
                    while (!release) {
                        try {
                            lock.wait();
                        } catch (InterruptedException e) {
                            return;
                        }
                    }
                }
            }, "w-" + i);
            w.start();
            waiters.add(w);
            // Arrival order: w-1 waits before w-2 starts.
            while (w.getState() != Thread.State.WAITING) {
                Thread.sleep(5);
            }
        }
        Thread owner = new Thread(() -> {
            synchronized (lock) {
                synchronized (lock) {
                    if (debugged) {
                        phase = 2;
                    }
                    while (debugged && phase != 3 && System.currentTimeMillis() < until) {
                        try {
                            Thread.sleep(10);
                        } catch (InterruptedException e) {
                            return;
                        }
                    }
                }
            }
        }, "owner");
        owner.start();
        owner.join();
        if (debugged) {
            phase = 4;
        }
        while (debugged && phase != 5 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        synchronized (lock) {
            release = true;
            lock.notifyAll();
        }
        for (Thread w : waiters) {
            w.join();
        }
        System.out.println("ended");
    }

    static final class Debugger {
        static VirtualMachine vm;
        static ClassType type;

        static void await(int value) throws Exception {
            long until = System.currentTimeMillis() + 120_000;
            while (((IntegerValue) type.getValue(type.fieldByName("phase"))).value() != value) {
                if (System.currentTimeMillis() > until) {
                    fail("phase " + value + " never came");
                }
                Thread.sleep(20);
            }
        }

        static void row(ObjectReference monitor) throws Exception {
            vm.suspend();
            try {
                ThreadReference owner = monitor.owningThread();
                List<String> waiting = new ArrayList<>();
                for (ThreadReference t : monitor.waitingThreads()) {
                    waiting.add(t.name());
                }
                Collections.sort(waiting);
                System.out.println("  owner=" + (owner == null ? "null" : owner.name())
                        + " entryCount=" + monitor.entryCount()
                        + " waiting=" + String.join(",", waiting));
            } finally {
                vm.resume();
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
            System.out.println("  canGetMonitorInfo=" + vm.canGetMonitorInfo());
            type = (ClassType) awaitClass("L1W103JdiMonitorInfo");
            ObjectReference monitor = (ObjectReference) type.getValue(type.fieldByName("lock"));
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(1));
            await(2);
            System.out.println("== held twice, two waiters");
            if (vm.canGetMonitorInfo()) {
                row(monitor);
            }
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(3));
            await(4);
            System.out.println("== free");
            if (vm.canGetMonitorInfo()) {
                row(monitor);
            }
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(5));
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
