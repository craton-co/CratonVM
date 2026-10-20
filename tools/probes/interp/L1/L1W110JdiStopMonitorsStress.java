// Interpreter round i5 wave 110, lane L1: a stress variant of
// L1W25JdiStopMonitors' `== monitors` section
// (docs/internal/fixed-bugs/interpreter-i110-L1-jdwp-owned-monitors-race-a-collection-FIXED-20261017.md).
//
// L1W25JdiStopMonitors failed 2 of 16 reruns on the Linux debug build w107
// (3 of 6 on w106) in its monitor rows. Read from the code: the owned-monitor
// reply for a thread blocked in a native region (the `owner`, asleep holding
// three monitors) resolved its frame attribution when it read the thread's
// inspection window -- on the JDWP server thread, no lock held, not a
// mutator -- and the owned set later, inside the quiesced reply. A
// collection between the two moved the locks: no pair matched an owned
// (remapped) address, and every monitor came out at depth -1, in lock-stack
// order (`owner owned=[LockA, LockC, LockB]`, `frames=[LockA@-1, ...]`). And
// a suspended thread caught between its window and its suspend point was
// answered from the registry at once, with the same shape.
//
// This variant repeats the reads ROUNDS times while a churn thread allocates
// (and asks for a collection every 100 ms), so collections fall between the
// reads; each row is printed once with how many rounds gave it. Any round
// that answered differently adds a row (the transcript then differs from
// HotSpot's). It also reads `ObjectReference.MonitorInfo` (owner, entry
// count, notify waiters) of the four locks each round, all threads but the
// churn suspended.
//
// Canonical transcript: no ids, no addresses, no timings.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   ended
//
// Under a debugger (the debuggee waits up to five minutes for the debugger):
//
//   javac -g -d out L1W110JdiStopMonitorsStress.java
//   cratonvm --java-home $JDK --jdwp-port 5796 -cp out L1W110JdiStopMonitorsStress wait
//   java -cp out L1W110JdiStopMonitorsStress debug 5796 > transcript.txt
//   # or: tools/jdi/run-jdi-conformance.sh --jdk $JDK --cratonvm $CVM \
//   #         --scenario L1W110JdiStopMonitorsStress   (and --check-headers first)
//
// HotSpot 25 as the debuggee (derived from L1W25JdiStopMonitors' measured
// transcript and L1W103JdiMonitorInfo's; NOT measured -- run
// `--check-headers` before listing it in the runner):
//
//   == attach
//   == stop at checkpoint thread=main
//   == monitors
//     x40 owner owned=[LockC, LockB, LockA]
//     x40 owner frames=[LockC@enter, LockB@enter, LockA@ownerRun]
//     x40 owner contended=null
//     x40 contender owned=[]
//     x40 contender contended=LockB
//     x40 waiter owned=[]
//     x40 waiter contended=null
//   == monitor info
//     x40 LockA owner=owner entryCount=1 waiting=[]
//     x40 LockB owner=owner entryCount=1 waiting=[contender]
//     x40 LockC owner=owner entryCount=1 waiting=[]
//     x40 WaitLock owner=null entryCount=0 waiting=[waiter]
//   == end
//   vm death
//   disconnected
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W110JdiStopMonitorsStress {
    static final int ROUNDS = 40;
    static volatile boolean attached;
    static volatile boolean holding;
    static volatile boolean waitDone;
    static volatile boolean churning = true;
    static volatile Object sink;

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
                try {
                    Thread.sleep(600_000);
                } catch (InterruptedException e) {
                    // Woken by main at the end.
                }
            }
        }
    }

    static final Object LOCK_A = new LockA();
    static final Object LOCK_B = new LockB();
    static final LockC LOCK_C = new LockC();
    static final Object WAIT_ON = new WaitLock();

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

    static void waiterRun() {
        synchronized (WAIT_ON) {
            while (!waitDone) {
                try {
                    WAIT_ON.wait();
                } catch (InterruptedException e) {
                    return;
                }
            }
        }
    }

    /** Young garbage with a few survivors, and a collection every 100 ms. */
    static void churnRun() {
        Object[] ring = new Object[64];
        long nextGc = System.currentTimeMillis() + 100;
        int i = 0;
        while (churning) {
            ring[i++ & 63] = new byte[2048 + (i & 1023)];
            sink = new int[64];
            if ((i & 1023) == 0 && System.currentTimeMillis() >= nextGc) {
                System.gc();
                nextGc = System.currentTimeMillis() + 100;
            }
        }
        sink = ring;
    }

    static void checkpoint(int count) {
        int twice = count * 2;
        if (twice < 0) {
            System.out.println("never");
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean debugged = args.length == 1 && args[0].equals("wait");
        if (debugged) {
            long until = System.currentTimeMillis() + 300_000;
            while (!attached) {
                if (System.currentTimeMillis() > until) {
                    System.out.println("no debugger attached");
                    System.exit(3);
                }
                Thread.sleep(20);
            }
        }
        Thread owner = new Thread(L1W110JdiStopMonitorsStress::ownerRun, "owner");
        Thread contender = new Thread(L1W110JdiStopMonitorsStress::contenderRun, "contender");
        Thread waiter = new Thread(L1W110JdiStopMonitorsStress::waiterRun, "waiter");
        Thread churn = new Thread(L1W110JdiStopMonitorsStress::churnRun, "churn");
        churn.setDaemon(true);
        owner.start();
        while (!holding) {
            Thread.sleep(1);
        }
        contender.start();
        waiter.start();
        churn.start();
        long settle = System.currentTimeMillis() + 10_000;
        while ((owner.getState() != Thread.State.TIMED_WAITING
                || contender.getState() != Thread.State.BLOCKED
                || waiter.getState() != Thread.State.WAITING)
                && System.currentTimeMillis() < settle) {
            Thread.sleep(1);
        }
        checkpoint(1);
        synchronized (WAIT_ON) {
            waitDone = true;
            WAIT_ON.notifyAll();
        }
        owner.interrupt();
        owner.join();
        contender.join();
        waiter.join();
        churning = false;
        churn.join();
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
            EventRequestManager erm = vm.eventRequestManager();
            System.out.println("== attach");
            ReferenceType type = awaitClass("L1W110JdiStopMonitorsStress");
            BreakpointRequest bp = erm.createBreakpointRequest(
                    type.methodsByName("checkpoint").get(0).location());
            bp.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            bp.enable();
            vm.resume();
            ((ClassType) type).setValue(type.fieldByName("attached"), vm.mirrorOf(true));
            awaitStop();

            Map<String, ThreadReference> byName = new HashMap<>();
            for (ThreadReference t : vm.allThreads()) {
                byName.put(t.name(), t);
            }
            Map<String, ObjectReference> locks = new LinkedHashMap<>();
            for (String field : new String[] {"LOCK_A", "LOCK_B", "LOCK_C", "WAIT_ON"}) {
                ObjectReference o = (ObjectReference) type.getValue(type.fieldByName(field));
                o.disableCollection();
                locks.put(simple(o), o);
            }

            Map<String, Integer> monitorRows = new LinkedHashMap<>();
            Map<String, Integer> infoRows = new LinkedHashMap<>();
            for (int round = 0; round < ROUNDS; round++) {
                for (String name : new String[] {"owner", "contender", "waiter"}) {
                    ThreadReference t = byName.get(name);
                    t.suspend();
                    try {
                        for (String row : monitorRows(name, t, name.equals("owner"))) {
                            monitorRows.merge(row, 1, Integer::sum);
                        }
                    } finally {
                        t.resume();
                    }
                }
                vm.suspend();
                try {
                    for (Map.Entry<String, ObjectReference> e : locks.entrySet()) {
                        ObjectReference m = e.getValue();
                        ThreadReference owner = m.owningThread();
                        List<String> waiting = new ArrayList<>();
                        for (ThreadReference w : m.waitingThreads()) {
                            waiting.add(w.name());
                        }
                        String row = e.getKey() + " owner=" + (owner == null ? "null" : owner.name())
                                + " entryCount=" + m.entryCount() + " waiting=" + waiting;
                        infoRows.merge(row, 1, Integer::sum);
                    }
                } finally {
                    vm.resume();
                }
                // Let the churn run (and collect) between rounds.
                Thread.sleep(5);
            }
            System.out.println("== monitors");
            monitorRows.forEach((row, n) -> System.out.println("  x" + n + " " + row));
            System.out.println("== monitor info");
            infoRows.forEach((row, n) -> System.out.println("  x" + n + " " + row));

            System.out.println("== end");
            for (ObjectReference o : locks.values()) {
                o.enableCollection();
            }
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

        static List<String> monitorRows(String name, ThreadReference t, boolean frames)
                throws Exception {
            List<String> rows = new ArrayList<>();
            List<String> owned = new ArrayList<>();
            for (ObjectReference o : t.ownedMonitors()) {
                owned.add(simple(o));
            }
            rows.add(name + " owned=" + owned);
            if (frames) {
                List<String> at = new ArrayList<>();
                for (MonitorInfo m : t.ownedMonitorsAndFrames()) {
                    int depth = m.stackDepth();
                    String where = depth < 0 ? "-1" : t.frame(depth).location().method().name();
                    at.add(simple(m.monitor()) + "@" + where);
                }
                rows.add(name + " frames=" + at);
            }
            rows.add(name + " contended=" + simple(t.currentContendedMonitor()));
            return rows;
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
