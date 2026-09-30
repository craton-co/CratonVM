// Interpreter round i1 wave 25, lane L1: the JDI conformance harness's
// `stop and monitors` scenario
// (docs/internal/fixed-bugs/interpreter-L1-jdwp-method-events-and-stop-miss-native-and-compiled-code-FIXED-20261005.md
// sections 1 and 3, and the owned-monitor commands of
// docs/known-issues/interpreter/i23-L7-proposal-per-frame-locked-monitors-20260926.md):
//
//  * ThreadReference.OwnedMonitors / OwnedMonitorsStackDepthInfo /
//    CurrentContendedMonitor (JDI ownedMonitors(), ownedMonitorsAndFrames(),
//    currentContendedMonitor()) for a thread holding three monitors at two
//    depths (a block, a synchronized method and a block in it), a thread
//    blocked entering one of them and a thread in Object.wait;
//  * ClassType.NewInstance of an abstract class (HotSpot: the invocation
//    throws InstantiationException) and of a concrete one;
//  * ThreadReference.Stop sent to three threads that are NOT suspended and
//    are blocked in Thread.sleep(60 s), LockSupport.park() and Object.wait():
//    each is woken at once and its own catch sees the debugger's throwable,
//    not an InterruptedException.
//
// What the transcript pins down about HotSpot: `ownedMonitors()` lists the
// frames innermost first but, within one frame, the monitors in the order
// they were entered (the synchronized method's `LockC` before the block's
// `LockB`) — JVMTI `GetOwnedMonitorInfo` walks `javaVFrame::monitors()`
// forwards, where `ThreadInfo.getLockedMonitors()` lists a frame's newest
// first; and a thread in `Object.wait` has no contended monitor (JDK 23+,
// JDK-8256314: only a monitor being entered is).
//
// Canonical transcript: no ids, no addresses, no timings (a woken thread
// reports `quick=true` when it saw the stop within 20 s of blocking).
// Run by the conformance runner, tools/jdi/run-jdi-conformance.sh
// (`--scenario L1W25JdiStopMonitors`).
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   sleeper=slept parker=unparked waiter=notified
//
// Under a debugger (the debuggee waits up to five minutes for the debugger to
// set the static `attached`):
//
//   javac -g -d out L1W25JdiStopMonitors.java
//   cratonvm --java-home $JDK --jdwp-port 5771 -cp out L1W25JdiStopMonitors wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5771 -cp out L1W25JdiStopMonitors wait)
//   java -cp out L1W25JdiStopMonitors debug 5771 > transcript.txt     # HotSpot 25's java
//
// HotSpot 25.0.3 as the debuggee (the debugger's stdout):
//
//   == attach
//   == stop at checkpoint thread=main
//   == capabilities
//     ownedMonitorInfo=true contendedMonitor=true monitorFrameInfo=true
//   == monitors
//     owner owned=[LockC, LockB, LockA]
//     owner frames=[LockC@enter, LockB@enter, LockA@ownerRun]
//     owner contended=null
//     contender owned=[]
//     contender frames=[]
//     contender contended=LockB
//     waiter owned=[]
//     waiter frames=[]
//     waiter contended=null
//     main owned=[]
//     main contended=null
//   == new instance
//     new Square() = instance of L1W25JdiStopMonitors$Square
//     new Shape() threw java.lang.InstantiationException
//   == stop
//     stops sent
//   == stop at report thread=main
//     sleeper saw java.lang.RuntimeException: stop-sleeper quick=true interrupted=false
//     parker saw java.lang.RuntimeException: stop-parker quick=true interrupted=true
//     waiter saw java.lang.RuntimeException: stop-waiter quick=true interrupted=false
//   == end
//   vm death
//   disconnected
//
// CratonVM before wave 25 (from reading the code; the orchestrator's run is
// the measurement): the capabilities line read `false false false` and the
// monitors section threw UnsupportedOperationException (the commands were
// not served); `new Shape()` was refused with INVALID_CLASS
// (`InternalException`); and each stopped thread stayed blocked until its
// own wake-up — the sleeper for the full 60 s (`quick=false`), the parker
// and the waiter until main unparked / notified them at the 30 s join
// timeout, after which each reported the stop.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;
import java.util.concurrent.locks.LockSupport;

public class L1W25JdiStopMonitors {
    static volatile boolean attached;
    static volatile boolean holding;
    static volatile boolean parked;
    static volatile boolean waitDone;
    static volatile String sleeperSaw = "none";
    static volatile String parkerSaw = "none";
    static volatile String waiterSaw = "none";
    /** 60 s under the debugger; a plain run does not wait that long. */
    static volatile long sleepMillis = 200;

    static final class LockA {
    }

    static final class LockB {
    }

    static final class WaitLock {
    }

    static final class LockC {
        synchronized void enter() {
            synchronized (LOCK_B) {
                holding = true;
                sleepQuietly(120_000);
            }
        }
    }

    abstract static class Shape {
        Shape() {
        }

        abstract int sides();
    }

    static final class Square extends Shape {
        Square() {
        }

        int sides() {
            return 4;
        }
    }

    static final Object LOCK_A = new LockA();
    static final Object LOCK_B = new LockB();
    static final LockC LOCK_C = new LockC();
    static final Object WAIT_ON = new WaitLock();

    static void sleepQuietly(long ms) {
        try {
            Thread.sleep(ms);
        } catch (InterruptedException e) {
            // Woken by main at the end.
        }
    }

    static String describe(Throwable t, long start) {
        long took = System.currentTimeMillis() - start;
        return t.getClass().getName() + ": " + t.getMessage() + " quick=" + (took < 20_000)
                + " interrupted=" + Thread.currentThread().isInterrupted();
    }

    static void ownerRun() {
        synchronized (LOCK_A) {
            LOCK_C.enter();
        }
    }

    static void contenderRun() {
        synchronized (LOCK_B) {
            holding = false;
        }
    }

    static void sleeperRun() {
        long start = System.currentTimeMillis();
        try {
            Thread.sleep(sleepMillis);
            sleeperSaw = "slept";
        } catch (Throwable t) {
            sleeperSaw = describe(t, start);
        }
    }

    static void parkerRun() {
        long start = System.currentTimeMillis();
        try {
            parked = true;
            LockSupport.park();
            parkerSaw = "unparked";
        } catch (Throwable t) {
            parkerSaw = describe(t, start);
        }
    }

    static void waiterRun() {
        long start = System.currentTimeMillis();
        try {
            synchronized (WAIT_ON) {
                while (!waitDone) {
                    WAIT_ON.wait();
                }
            }
            waiterSaw = "notified";
        } catch (Throwable t) {
            waiterSaw = describe(t, start);
        }
    }

    static void checkpoint(int count) {
        int twice = count * 2;
        if (twice < 0) {
            System.out.println("never");
        }
    }

    static void report(String sleeper, String parker, String waiter) {
        System.out.println("sleeper=" + sleeper + " parker=" + parker + " waiter=" + waiter);
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean debugged = args.length == 1 && args[0].equals("wait");
        if (debugged) {
            sleepMillis = 60_000;
            waitForDebugger();
        }
        if (new Square().sides() != 4) {
            System.out.println("bad square");
        }
        Thread owner = new Thread(L1W25JdiStopMonitors::ownerRun, "owner");
        Thread contender = new Thread(L1W25JdiStopMonitors::contenderRun, "contender");
        Thread waiter = new Thread(L1W25JdiStopMonitors::waiterRun, "waiter");
        Thread sleeper = new Thread(L1W25JdiStopMonitors::sleeperRun, "sleeper");
        Thread parker = new Thread(L1W25JdiStopMonitors::parkerRun, "parker");
        owner.start();
        while (!holding) {
            Thread.sleep(1);
        }
        contender.start();
        waiter.start();
        sleeper.start();
        parker.start();
        long settle = System.currentTimeMillis() + 10_000;
        while ((owner.getState() != Thread.State.TIMED_WAITING
                || contender.getState() != Thread.State.BLOCKED
                || waiter.getState() != Thread.State.WAITING
                || sleeper.getState() != Thread.State.TIMED_WAITING
                || parker.getState() != Thread.State.WAITING
                || !parked)
                && System.currentTimeMillis() < settle) {
            Thread.sleep(1);
        }
        checkpoint(1);
        if (!debugged) {
            // No debugger: end each blocked thread the ordinary way.
            sleeper.join();
            LockSupport.unpark(parker);
            synchronized (WAIT_ON) {
                waitDone = true;
                WAIT_ON.notifyAll();
            }
        }
        // Under the debugger each thread ends by the stop it was sent; the
        // fallbacks below end it anyway (the transcript then differs).
        for (Thread t : new Thread[] {sleeper, parker, waiter}) {
            t.join(30_000);
        }
        LockSupport.unpark(parker);
        synchronized (WAIT_ON) {
            waitDone = true;
            WAIT_ON.notifyAll();
        }
        sleeper.join();
        parker.join();
        waiter.join();
        report(sleeperSaw, parkerSaw, waiterSaw);
        owner.interrupt();
        owner.join();
        contender.join();
    }

    static void waitForDebugger() throws InterruptedException {
        long until = System.currentTimeMillis() + 300_000;
        while (!attached) {
            if (System.currentTimeMillis() > until) {
                System.out.println("no debugger attached");
                System.exit(3);
            }
            Thread.sleep(20);
        }
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
            ReferenceType type = awaitClass("L1W25JdiStopMonitors");
            BreakpointRequest bp = erm.createBreakpointRequest(
                    type.methodsByName("checkpoint").get(0).location());
            bp.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            bp.enable();
            BreakpointRequest atReport = erm.createBreakpointRequest(
                    type.methodsByName("report").get(0).location());
            atReport.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            atReport.enable();
            // The (no-op) resume first: sent after the flag, it could reach a
            // slow back end after the program had run into its first
            // SUSPEND_ALL stop, and release that stop.
            vm.resume();
            ((ClassType) type).setValue(type.fieldByName("attached"), vm.mirrorOf(true));
            ThreadReference main = awaitStop();

            System.out.println("== capabilities");
            System.out.println("  ownedMonitorInfo=" + vm.canGetOwnedMonitorInfo()
                    + " contendedMonitor=" + vm.canGetCurrentContendedMonitor()
                    + " monitorFrameInfo=" + vm.canGetMonitorFrameInfo());

            Map<String, ThreadReference> byName = new HashMap<>();
            for (ThreadReference t : vm.allThreads()) {
                byName.put(t.name(), t);
            }

            System.out.println("== monitors");
            for (String name : new String[] {"owner", "contender", "waiter"}) {
                ThreadReference t = byName.get(name);
                t.suspend();
                try {
                    printMonitors(name, t, true);
                } finally {
                    t.resume();
                }
            }
            printMonitors("main", main, false);

            System.out.println("== new instance");
            ClassType square = (ClassType) vm.classesByName("L1W25JdiStopMonitors$Square").get(0);
            ObjectReference made = square.newInstance(main, square.methodsByName("<init>").get(0),
                    List.of(), 0);
            System.out.println("  new Square() = instance of " + made.referenceType().name());
            ClassType shape = (ClassType) vm.classesByName("L1W25JdiStopMonitors$Shape").get(0);
            try {
                ObjectReference bad = shape.newInstance(main, shape.methodsByName("<init>").get(0),
                        List.of(), 0);
                System.out.println("  new Shape() = instance of " + bad.referenceType().name());
            } catch (InvocationException e) {
                System.out.println("  new Shape() threw " + e.exception().referenceType().name());
            } catch (Exception e) {
                System.out.println("  new Shape() refused: " + e.getClass().getName());
            }

            System.out.println("== stop");
            ClassType rte = (ClassType) vm.classesByName("java.lang.RuntimeException").get(0);
            Method ctor = rte.concreteMethodByName("<init>", "(Ljava/lang/String;)V");
            List<ObjectReference> keep = new ArrayList<>();
            for (String name : new String[] {"sleeper", "parker", "waiter"}) {
                ObjectReference exc = rte.newInstance(main, ctor,
                        List.of(vm.mirrorOf("stop-" + name)), 0);
                exc.disableCollection();
                keep.add(exc);
                byName.get(name).stop(exc);
            }
            System.out.println("  stops sent");
            vm.resume();
            awaitStop();
            for (String field : new String[] {"sleeperSaw", "parkerSaw", "waiterSaw"}) {
                Value v = type.getValue(type.fieldByName(field));
                String text = v instanceof StringReference s ? s.value() : String.valueOf(v);
                System.out.println("  " + field.replace("Saw", "") + " saw " + text);
            }

            System.out.println("== end");
            erm.deleteAllBreakpoints();
            vm.resume();
            while (true) {
                EventSet set;
                try {
                    set = vm.eventQueue().remove(120_000);
                } catch (VMDisconnectedException gone) {
                    System.out.println("disconnected");
                    return;
                }
                if (set == null) {
                    fail("the debuggee did not end");
                }
                for (Event e : set) {
                    if (e instanceof VMDeathEvent) {
                        System.out.println("vm death");
                    } else if (e instanceof VMDisconnectEvent) {
                        System.out.println("disconnected");
                        return;
                    }
                }
                set.resume();
            }
        }

        static String simple(ObjectReference o) {
            if (o == null) {
                return "null";
            }
            String n = o.referenceType().name();
            return n.substring(n.lastIndexOf('$') + 1);
        }

        static void printMonitors(String name, ThreadReference t, boolean frames) throws Exception {
            List<String> owned = new ArrayList<>();
            for (ObjectReference o : t.ownedMonitors()) {
                owned.add(simple(o));
            }
            System.out.println("  " + name + " owned=" + owned);
            if (frames) {
                List<String> at = new ArrayList<>();
                for (MonitorInfo m : t.ownedMonitorsAndFrames()) {
                    int depth = m.stackDepth();
                    String where = depth < 0 ? "-1" : t.frame(depth).location().method().name();
                    at.add(simple(m.monitor()) + "@" + where);
                }
                System.out.println("  " + name + " frames=" + at);
            }
            System.out.println("  " + name + " contended=" + simple(t.currentContendedMonitor()));
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

        static ThreadReference awaitStop() throws Exception {
            long until = System.currentTimeMillis() + 180_000;
            while (System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(500);
                if (set == null) {
                    continue;
                }
                for (Event e : set) {
                    if (e instanceof BreakpointEvent b) {
                        System.out.println("== stop at " + b.location().method().name()
                                + " thread=" + b.thread().name());
                        return b.thread();
                    }
                    if (e instanceof VMDeathEvent || e instanceof VMDisconnectEvent) {
                        fail("the debuggee ended early");
                    }
                }
                set.resume();
            }
            fail("no breakpoint");
            return null;
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
