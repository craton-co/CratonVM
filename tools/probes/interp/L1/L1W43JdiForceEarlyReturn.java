// Interpreter round i1 wave 43, lane L1: `ThreadReference.ForceEarlyReturn`
// (JDWP 11/14) and `canForceEarlyReturn`, stage 2 of
// docs/internal/fixed-bugs/interpreter-L1-proposal-pop-frames-force-early-return-and-source-debug-extension-FIXED-20261008.md
// (an IDE's "Force Return").
//
// The debuggee's `main` waits (`go`) until the debugger has set a breakpoint
// (SUSPEND_EVENT_THREAD) at the first bytecode of each of `answer(int)`,
// `name()`, `effect()`, `big(long)`, `ratio()`, `locked()` (a synchronized
// method) and at the line inside `block()`'s `synchronized (LOCK)` block,
// then calls each once. At each stop the debugger forces the method to
// return a value of its own choosing and resumes; a MethodExit request on
// the probe class reports each forced exit with its value. `main` then
// checks, from a second thread, that the monitors of `locked()` and
// `block()` were released, and parks in `finish()`, where the debugger reads
// what each call returned. Before the first call, the debugger forces a
// return on a thread blocked in `Object.wait` (its top frame is a native
// method) and on one it has not suspended.
//
// Canonical transcript: no ids, no addresses, no timings.
// Run by the conformance runner, tools/jdi/run-jdi-conformance.sh
// (`--scenario L1W43JdiForceEarlyReturn`).
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   answer=41 name=real effects=1 big=6 ratio=0.5 locked=1 block=2 free=true
//
// Under a debugger (the debuggee waits up to five minutes):
//
//   javac -g -d out L1W43JdiForceEarlyReturn.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L1W43JdiForceEarlyReturn wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5791 -cp out L1W43JdiForceEarlyReturn wait)
//   java -cp out L1W43JdiForceEarlyReturn debug 5791 > transcript.txt     # HotSpot 25's java
//
// HotSpot 25.0.3 as the debuggee (the debugger's stdout; three runs, the
// same each time):
//
//   == attach
//   canForceEarlyReturn: true
//   == refused
//     top frame of a waiting thread is native: true
//     waiting thread: NativeMethodException
//     running thread: IncompatibleThreadStateException
//   == forced returns
//     answer: ok
//     exit answer = 99
//     name: ok
//     exit name = "forced"
//     effect: ok
//     exit effect = <void value>
//     big: ok
//     exit big = 1099511627776
//     ratio: ok
//     exit ratio = 2.25
//     locked: ok
//     exit locked = 7
//     block: ok
//     exit block = 8
//   == results
//     answerResult = 99
//     nameResult = "forced"
//     effects = 0
//     bigResult = 1099511627776
//     ratioResult = 2.25
//     lockedResult = 7
//     blockResult = 8
//     free = true
//   == end
//     disconnected
//
// (Measured on the Windows box; the orchestrator should take the host's
// HotSpot run as the reference.) The debuggee then prints
// `answer=99 name=forced effects=0 big=1099511627776 ratio=2.25 locked=7
// block=8 free=true`. `running thread` is refused by JDI itself (it lists
// the frames first); `waiting thread` reaches the VM: HotSpot answers
// `OPAQUE_FRAME`, which JDI throws as `NativeMethodException`.
//
// CratonVM before wave 43 answered `canForceEarlyReturn` false
// (`commands::handle_vm_capabilities_new`), so every `force` printed
// `UnsupportedOperationException` (JDI refuses without sending), no exit
// was forced and `== results` printed the real values (`answerResult = 41`
// and so on). Since wave 43 the command is recorded on the parked thread and
// the frame returns the value when the thread resumes
// (`debug::early_return`, `interpreter::force_return_here`,
// `interpreter::finish_forced_return`). `free = true` checks that the
// monitors of the `synchronized` method and of the `synchronized` block were
// released. Positive control: the `exit <name> = <value>` lines are the
// forced returns' own `MethodExit` events, which the base cannot print.
// Every CratonVM mode must print the same.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W43JdiForceEarlyReturn {
    /** 0 until the debugger has set its breakpoints. */
    static volatile int go;
    static final Object LOCK = new Object();
    /** The monitor a helper thread waits on, never notified until the end. */
    static final Object PARK = new Object();
    static volatile boolean parkDone;
    static int effects;
    static int answerResult;
    static String nameResult;
    static long bigResult;
    static double ratioResult;
    static int lockedResult;
    static int blockResult;
    static boolean free;

    static int answer(int x) {
        int y = x * 2;
        return y + 1;
    }

    static String name() {
        return "real";
    }

    static void effect() {
        effects++;
    }

    static long big(long x) {
        return x + 1;
    }

    static double ratio() {
        return 0.5;
    }

    static synchronized int locked() {
        return 1;
    }

    static int block() {
        int r = 0;
        synchronized (LOCK) {
            r = 2; // BLOCK_LINE
        }
        return r;
    }

    /** The debugger reads the results here (a breakpoint at its first bytecode). */
    static void finish() {
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean wait = args.length == 1 && args[0].equals("wait");
        Thread parked = new Thread(() -> {
            synchronized (PARK) {
                while (!parkDone) {
                    try {
                        PARK.wait();
                    } catch (InterruptedException stop) {
                        return;
                    }
                }
            }
        }, "parked");
        parked.setDaemon(true);
        parked.start();
        if (wait) {
            long until = System.currentTimeMillis() + 300_000;
            while (go == 0 && System.currentTimeMillis() < until) {
                Thread.sleep(10);
            }
        }
        answerResult = answer(20);
        nameResult = name();
        effect();
        bigResult = big(5);
        ratioResult = ratio();
        lockedResult = locked();
        blockResult = block();
        // Both monitors are free for another thread.
        boolean[] took = new boolean[1];
        Thread other = new Thread(() -> {
            synchronized (LOCK) {
                synchronized (L1W43JdiForceEarlyReturn.class) {
                    took[0] = true;
                }
            }
        }, "other");
        other.setDaemon(true);
        other.start();
        // Wave 46 (lane L1): what `start()` left behind, read before the
        // join, for the diagnostic below. `start()` returns only once the
        // thread is alive (java.lang.Thread), and `join` waits for it to end,
        // so on a correct VM `other` has run by the time `free` is read,
        // whatever the scheduling: the check is not racy.
        boolean aliveAfterStart = other.isAlive();
        long joinStart = System.nanoTime();
        other.join(10_000);
        long joinMs = (System.nanoTime() - joinStart) / 1_000_000;
        free = took[0];
        if (!free) {
            // A diagnostic for a failing run only (HotSpot never prints it):
            // which monitor `main` itself still holds, and whether `other`
            // is still waiting for one. Wave 46: whether `other` was alive
            // right after `start()`, how long `join` waited (at once: the VM
            // did not see it alive; ten seconds: it never ended), and, half
            // a second later, its state and whether it did run.
            String late;
            try {
                Thread.sleep(500);
                late = " later: other=" + other.getState() + " alive=" + other.isAlive()
                        + " took=" + took[0];
            } catch (InterruptedException stop) {
                late = " later: interrupted";
            }
            System.err.println("not free: main holds LOCK=" + Thread.holdsLock(LOCK)
                    + " class=" + Thread.holdsLock(L1W43JdiForceEarlyReturn.class)
                    + " other=" + other.getState() + " aliveAfterStart=" + aliveAfterStart
                    + " joinMs=" + joinMs + late);
        }
        finish();
        synchronized (PARK) {
            parkDone = true;
            PARK.notifyAll();
        }
        System.out.println("answer=" + answerResult + " name=" + nameResult + " effects=" + effects
                + " big=" + bigResult + " ratio=" + ratioResult + " locked=" + lockedResult
                + " block=" + blockResult + " free=" + free);
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
            System.out.println("canForceEarlyReturn: " + vm.canForceEarlyReturn());
            ClassType type = (ClassType) awaitClass("L1W43JdiForceEarlyReturn");
            ThreadReference main = thread("main");
            ThreadReference parked = thread("parked");

            System.out.println("== refused");
            // `parked` waits in `Object.wait`: a native method is on top.
            long until = System.currentTimeMillis() + 60_000;
            while (parked.status() != ThreadReference.THREAD_STATUS_WAIT
                    && System.currentTimeMillis() < until) {
                Thread.sleep(20);
            }
            parked.suspend();
            System.out.println("  top frame of a waiting thread is native: "
                    + parked.frame(0).location().method().isNative());
            System.out.println("  waiting thread: " + force(parked, vm.mirrorOfVoid()));
            parked.resume();
            // `main` runs (sleeps in a loop): JDI refuses before sending.
            System.out.println("  running thread: " + force(main, vm.mirrorOf(1)));

            List<String> names = List.of("answer", "name", "effect", "big", "ratio", "locked");
            for (String n : names) {
                Method m = type.methodsByName(n).get(0);
                BreakpointRequest bp = erm.createBreakpointRequest(m.location());
                bp.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
                bp.putProperty("name", n);
                bp.enable();
            }
            Method block = type.methodsByName("block").get(0);
            // The line of `r = 2;`: the third distinct line of `block`.
            TreeSet<Integer> lines = new TreeSet<>();
            for (Location l : block.allLineLocations()) {
                lines.add(l.lineNumber());
            }
            int blockLine = new ArrayList<>(lines).get(2);
            BreakpointRequest inBlock = erm.createBreakpointRequest(block.locationsOfLine(blockLine).get(0));
            inBlock.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            inBlock.putProperty("name", "block");
            inBlock.enable();
            BreakpointRequest fin = erm.createBreakpointRequest(
                    type.methodsByName("finish").get(0).location());
            fin.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            fin.putProperty("name", "finish");
            fin.enable();
            MethodExitRequest exits = erm.createMethodExitRequest();
            exits.addClassFilter(type);
            exits.addThreadFilter(main);
            exits.setSuspendPolicy(EventRequest.SUSPEND_NONE);
            exits.enable();

            System.out.println("== forced returns");
            type.setValue(type.fieldByName("go"), vm.mirrorOf(1));
            boolean ended = false;
            boolean finished = false;
            until = System.currentTimeMillis() + 120_000;
            while (!finished && !ended && System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(5_000);
                if (set == null) {
                    continue;
                }
                for (Event e : set) {
                    if (e instanceof BreakpointEvent b) {
                        String n = (String) b.request().getProperty("name");
                        ThreadReference t = b.thread();
                        switch (n) {
                            case "answer" -> System.out.println("  answer: " + force(t, vm.mirrorOf(99)));
                            case "name" -> System.out.println("  name: " + force(t, vm.mirrorOf("forced")));
                            case "effect" -> System.out.println("  effect: " + force(t, vm.mirrorOfVoid()));
                            case "big" -> System.out.println("  big: " + force(t, vm.mirrorOf(1L << 40)));
                            case "ratio" -> System.out.println("  ratio: " + force(t, vm.mirrorOf(2.25)));
                            case "locked" -> System.out.println("  locked: " + force(t, vm.mirrorOf(7)));
                            case "block" -> System.out.println("  block: " + force(t, vm.mirrorOf(8)));
                            case "finish" -> {
                                finished = true;
                                System.out.println("== results");
                                for (String f : List.of("answerResult", "nameResult", "effects",
                                        "bigResult", "ratioResult", "lockedResult", "blockResult", "free")) {
                                    System.out.println("  " + f + " = " + type.getValue(type.fieldByName(f)));
                                }
                            }
                            default -> System.out.println("  unexpected stop in " + n);
                        }
                    } else if (e instanceof MethodExitEvent x) {
                        String n = x.method().name();
                        if (!n.equals("main") && !n.equals("finish") && !n.startsWith("lambda$")) {
                            System.out.println("  exit " + n + " = " + x.returnValue());
                        }
                    } else if (e instanceof VMDeathEvent || e instanceof VMDisconnectEvent) {
                        ended = true;
                    }
                }
                if (!ended) {
                    set.resume();
                }
            }
            if (!finished) {
                fail("never stopped in finish");
            }

            System.out.println("== end");
            erm.deleteAllBreakpoints();
            erm.deleteEventRequest(exits);
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

        /** Force the top frame of `t` to return `v`: "ok" or the exception's name. */
        static String force(ThreadReference t, Value v) {
            try {
                t.forceEarlyReturn(v);
                return "ok";
            } catch (Exception e) {
                return e.getClass().getSimpleName();
            }
        }

        static ThreadReference thread(String name) throws Exception {
            long until = System.currentTimeMillis() + 120_000;
            while (true) {
                for (ThreadReference t : vm.allThreads()) {
                    if (t.name().equals(name)) {
                        return t;
                    }
                }
                if (System.currentTimeMillis() > until) {
                    fail("no thread " + name);
                }
                Thread.sleep(50);
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
