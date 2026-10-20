// Interpreter round i1 wave 24, lane L1: the JDI conformance harness's
// `threads` scenario (stage 1, second half, of
// docs/internal/fixed-bugs/interpreter-L1-proposal-jdi-conformance-harness-FIXED-20261003.md):
// what jdb's `threads`, `where` and `locals` show for a program with a
// sleeping, a waiting, a monitor-blocked, a sleeping-while-holding and a
// finished thread, stopped at a breakpoint with SUSPEND_ALL — thread
// statuses, suspension and suspend counts, thread groups (the program's
// `main` under `system`), a terminated thread held in a field (a
// ThreadReference whose status is ZOMBIE), frames of blocked threads, a
// local, and suspend / resume counting with `frames()` refused for a running
// thread. Canonical transcript: no ids, no addresses, only the program's own
// frames. Run by the conformance runner, tools/jdi/run-jdi-conformance.sh.
//
// Plain run (what the probe runner diffs; no debugger): stdout must equal
// HotSpot 25's:
//
//   checkpoint 4 8
//   threads done
//
// Under a debugger (the debuggee waits up to five minutes for the debugger to
// set the static `attached`):
//
//   javac -g -d out L1W24JdiThreads.java
//   cratonvm --java-home $JDK --jdwp-port 5761 -cp out L1W24JdiThreads wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5761 -cp out L1W24JdiThreads wait)
//   java -cp out L1W24JdiThreads debug 5761 > transcript.txt     # HotSpot 25's java
//
// HotSpot 25.0.3 as the debuggee (the debugger's stdout):
//
//   == attach
//   == stop at checkpoint:89 thread=main
//   == threads
//     blocked status=MONITOR suspended=true count=1 group=main
//     holder status=SLEEPING suspended=true count=1 group=main
//     main status=RUNNING suspended=true count=1 group=main
//     sleeper status=SLEEPING suspended=true count=1 group=main
//     waiter status=WAIT suspended=true count=1 group=main
//     listed finished=false
//     finished: ThreadReference name=finished status=ZOMBIE
//   == groups
//     top system parent=null
//     child main parent=system threads=[blocked, holder, main, sleeper, waiter]
//   == where
//     main frames>0=true count=true
//       checkpoint:89
//       main:139
//     waiter frames>0=true count=true
//       lambda$main$2:109
//     blocked frames>0=true count=true
//       lambda$main$4:123
//     main local count = 4
//   == suspend counts
//     after suspend count=2
//     after two resumes count=0 suspended=false status=SLEEPING
//     frames of a running thread: IncompatibleThreadStateException
//     suspended again count=1
//   == end
//   vm death
//   disconnected
//
// CratonVM before wave 24 (from reading the code; the orchestrator's run is
// the measurement): every group line read `system` (one modelled group, no
// `main`, `top-level` parent-less `system` holding every thread), the held
// `finished` value was a plain ObjectReference, and no `vm death` line
// printed.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W24JdiThreads {
    static volatile boolean attached;
    static volatile boolean holding;
    static final Object LOCK = new Object();
    static final Object MONITOR = new Object();
    static Thread finishedRef;

    static void sleepQuietly(long ms) {
        try {
            Thread.sleep(ms);
        } catch (InterruptedException e) {
            // Woken by main at the end.
        }
    }

    static void checkpoint(int count) {
        int twice = count * 2;
        System.out.println("checkpoint " + count + " " + twice);
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        if (args.length == 1 && args[0].equals("wait")) {
            waitForDebugger();
        }
        Thread finished = new Thread(() -> { }, "finished");
        finished.start();
        finished.join();
        finishedRef = finished;
        Thread sleeper = new Thread(() -> sleepQuietly(120_000), "sleeper");
        Thread waiter = new Thread(() -> {
            synchronized (LOCK) {
                try {
                    LOCK.wait();
                } catch (InterruptedException e) {
                    // Woken by main at the end.
                }
            }
        }, "waiter");
        Thread holder = new Thread(() -> {
            synchronized (MONITOR) {
                holding = true;
                sleepQuietly(120_000);
            }
        }, "holder");
        Thread blocked = new Thread(() -> {
            synchronized (MONITOR) {
                holding = false;
            }
        }, "blocked");
        sleeper.start();
        waiter.start();
        holder.start();
        while (!holding) {
            Thread.sleep(1);
        }
        blocked.start();
        while (sleeper.getState() != Thread.State.TIMED_WAITING
                || waiter.getState() != Thread.State.WAITING
                || holder.getState() != Thread.State.TIMED_WAITING
                || blocked.getState() != Thread.State.BLOCKED) {
            if (!settle()) break;
        }
        checkpoint(4);
        sleeper.interrupt();
        holder.interrupt();
        synchronized (LOCK) {
            LOCK.notifyAll();
        }
        for (Thread t : new Thread[] {sleeper, waiter, holder, blocked}) {
            t.join();
        }
        System.out.println("threads done");
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
        static final List<String> OURS = List.of("blocked", "holder", "main", "sleeper", "waiter");
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
            ReferenceType type = awaitClass("L1W24JdiThreads");
            BreakpointRequest bp = erm.createBreakpointRequest(
                    type.methodsByName("checkpoint").get(0).location());
            bp.setSuspendPolicy(EventRequest.SUSPEND_ALL);
            bp.enable();
            // The (no-op) resume first: sent after the flag, it could reach a
            // slow back end after the program had run into its first
            // SUSPEND_ALL stop, and release that stop.
            vm.resume();
            ((ClassType) type).setValue(type.fieldByName("attached"), vm.mirrorOf(true));
            ThreadReference main = awaitStop();

            System.out.println("== threads");
            Map<String, ThreadReference> byName = new TreeMap<>();
            for (ThreadReference t : vm.allThreads()) {
                if (OURS.contains(t.name())) {
                    byName.put(t.name(), t);
                }
            }
            for (ThreadReference t : byName.values()) {
                ThreadGroupReference g = t.threadGroup();
                System.out.println("  " + t.name() + " status=" + status(t) + " suspended="
                        + t.isSuspended() + " count=" + t.suspendCount()
                        + " group=" + (g == null ? "null" : g.name()));
            }
            System.out.println("  listed finished=" + names(vm.allThreads()).contains("finished"));
            Value finished = type.getValue(type.fieldByName("finishedRef"));
            if (finished instanceof ThreadReference f) {
                System.out.println("  finished: ThreadReference name=" + f.name() + " status=" + status(f));
            } else {
                System.out.println("  finished: " + (finished == null ? "null" : finished.getClass().getSimpleName()));
            }

            System.out.println("== groups");
            for (ThreadGroupReference top : vm.topLevelThreadGroups()) {
                System.out.println("  top " + top.name() + " parent="
                        + (top.parent() == null ? "null" : top.parent().name()));
                for (ThreadGroupReference sub : top.threadGroups()) {
                    if (sub.name().equals("main")) {
                        List<String> ours = new ArrayList<>();
                        for (String n : names(sub.threads())) {
                            if (OURS.contains(n)) {
                                ours.add(n);
                            }
                        }
                        Collections.sort(ours);
                        System.out.println("  child main parent=" + sub.parent().name() + " threads=" + ours);
                    }
                }
            }

            System.out.println("== where");
            for (String name : new String[] {"main", "waiter", "blocked"}) {
                ThreadReference t = byName.get(name);
                List<StackFrame> frames = t.frames();
                System.out.println("  " + name + " frames>0=" + !frames.isEmpty() + " count="
                        + (t.frameCount() == frames.size()));
                for (StackFrame f : frames) {
                    Location l = f.location();
                    if (l.declaringType().name().equals("L1W24JdiThreads")) {
                        System.out.println("    " + l.method().name() + ":" + l.lineNumber());
                    }
                }
            }
            StackFrame top = main.frame(0);
            for (LocalVariable v : top.visibleVariables()) {
                System.out.println("  main local " + v.name() + " = " + top.getValue(v));
            }

            System.out.println("== suspend counts");
            ThreadReference sleeper = byName.get("sleeper");
            sleeper.suspend();
            System.out.println("  after suspend count=" + sleeper.suspendCount());
            sleeper.resume();
            sleeper.resume();
            System.out.println("  after two resumes count=" + sleeper.suspendCount()
                    + " suspended=" + sleeper.isSuspended() + " status=" + status(sleeper));
            try {
                sleeper.frames();
                System.out.println("  frames of a running thread answered");
            } catch (IncompatibleThreadStateException e) {
                System.out.println("  frames of a running thread: IncompatibleThreadStateException");
            }
            sleeper.suspend();
            System.out.println("  suspended again count=" + sleeper.suspendCount());

            System.out.println("== end");
            erm.deleteAllBreakpoints();
            vm.resume();
            while (true) {
                EventSet set;
                try {
                    set = vm.eventQueue().remove(60_000);
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

        static List<String> names(List<ThreadReference> threads) {
            List<String> out = new ArrayList<>();
            for (ThreadReference t : threads) {
                out.add(t.name());
            }
            return out;
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
            long until = System.currentTimeMillis() + 120_000;
            while (System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(500);
                if (set == null) {
                    continue;
                }
                for (Event e : set) {
                    if (e instanceof BreakpointEvent b) {
                        Location l = b.location();
                        System.out.println("== stop at " + l.method().name() + ":" + l.lineNumber()
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

    /** A bounded wait (ten seconds) for the threads to reach their states. */
    static long settleDeadline;

    static boolean settle() throws InterruptedException {
        if (settleDeadline == 0) {
            settleDeadline = System.currentTimeMillis() + 10_000;
        }
        Thread.sleep(1);
        return System.currentTimeMillis() < settleDeadline;
    }
}
