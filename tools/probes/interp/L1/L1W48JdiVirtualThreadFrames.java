// Interpreter round i1 wave 48, lane L1: the frames a debugger lists for a
// VIRTUAL thread blocked in `Thread.sleep`, `LockSupport.parkNanos` and
// `Object.wait`
// (docs/internal/fixed-bugs/interpreter-L1-an-unmounted-virtual-thread-lists-no-frames-FIXED-20261012.md;
// what remains:
// docs/internal/fixed-bugs/interpreter-L1-an-unmounted-virtual-thread-lists-its-bound-frames-not-the-continuation-frames-FIXED-20261013.md).
//
// The debuggee starts three virtual threads once the debugger has attached
// (the debugger learns them from `ThreadStart` events: HotSpot's
// `AllThreads` leaves virtual threads out unless the agent was started with
// `includevirtualthreads=y`): `vsleeper` in `Thread.sleep(LONG)`, `vparker`
// in `LockSupport.parkNanos(LONG)`, `vwaiter` in `W.wait(LONG)` inside
// `synchronized (W)`. Once all three block, the debugger suspends the VM and
// prints each one's frames, top first, down to the probe's own frame.
//
// Canonical transcript: no ids, no addresses, no timings.
// Run by the conformance runner, tools/jdi/run-jdi-conformance.sh
// (`--scenario L1W48JdiVirtualThreadFrames`).
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   ended
//
// Under a debugger:
//
//   javac -g -d out L1W48JdiVirtualThreadFrames.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L1W48JdiVirtualThreadFrames wait
//   java -cp out L1W48JdiVirtualThreadFrames debug 5791 > transcript.txt
//
// HotSpot 25.0.3 as the debuggee (default flags, the Windows box):
//
//   == attach
//   == suspended
//     vsleeper virtual: true
//     vsleeper frames: java.lang.VirtualThread.yieldContinuation java.lang.VirtualThread.parkNanos java.lang.VirtualThread.sleepNanos java.lang.Thread.sleepNanos java.lang.Thread.sleep L1W48JdiVirtualThreadFrames.lambda$main$0
//     vparker virtual: true
//     vparker frames: java.lang.VirtualThread.yieldContinuation java.lang.VirtualThread.parkNanos java.lang.System$1.parkVirtualThread java.util.concurrent.locks.LockSupport.parkNanos L1W48JdiVirtualThreadFrames.lambda$main$2
//     vwaiter virtual: true
//     vwaiter frames: java.lang.Object.wait0[native] java.lang.Object.wait L1W48JdiVirtualThreadFrames.lambda$main$1
//   == end
//     disconnected
//
// HotSpot run without continuations (`-XX:+UnlockExperimentalVMOptions
// -XX:-VMContinuations`: its virtual threads are `BoundVirtualThread`s, as
// this VM's are) lists `vsleeper frames: java.lang.Thread.sleepNanos0[native]
// java.lang.Thread.sleepNanos java.lang.Thread.sleep ...` and `vparker
// frames: jdk.internal.misc.Unsafe.park[native]
// java.lang.ThreadBuilders$BoundVirtualThread.parkNanos
// java.lang.System$1.parkVirtualThread
// java.util.concurrent.locks.LockSupport.parkNanos ...`, which is what
// CratonVM lists since wave 48 under `--jdk-only` (`--compatible`'s
// `vparker`: `jdk.internal.misc.Unsafe.park[native]
// java.util.concurrent.locks.LockSupport.parkNanos ...`);
// Since wave 49 (lane L1) CratonVM lists the default rows above in all four
// modes (`CRATONVM_DEBUG_VIRTUAL_CONTINUATION_FRAMES`,
// `interpreter::continuation_head_of_unmounted`); the known-differences
// lines are gone.
//
// CratonVM before wave 48 (Windows `livedbg` build of `0eed88a3a`, both
// modes): `vsleeper frames:` and `vparker frames:` listed nothing at all --
// an unmounted continuation published no frames: it is neither at an
// interpreter suspend point nor in a blocking region. `vwaiter` (pinned,
// blocked in its carrier) matched. Positive control: `CRATONVM_FRAME_TRACE=1`
// prints `[UNMOUNTED_VT_FRAMES] tid=<n> frames=<k> head=true` once for each
// of `vsleeper` and `vparker`; the base prints no such line.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;
import java.util.concurrent.locks.LockSupport;

public class L1W48JdiVirtualThreadFrames {
    /** Set by the debugger: 1 = start the threads, 3 = done. Set to 2 by the debuggee once they block. */
    static volatile int phase;
    static final Object W = new Object();
    static final long LONG = 600_000L;

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
        Thread sleeper = Thread.ofVirtual().name("vsleeper").start(() -> {
            try {
                Thread.sleep(LONG);
            } catch (InterruptedException stop) {
                // Ends with the program.
            }
        });
        Thread waiter = Thread.ofVirtual().name("vwaiter").start(() -> {
            synchronized (W) {
                try {
                    W.wait(LONG);
                } catch (InterruptedException stop) {
                    // Ends with the program.
                }
            }
        });
        Thread parker = Thread.ofVirtual().name("vparker").start(() -> {
            LockSupport.parkNanos(LONG * 1_000_000L);
        });
        Thread[] all = {sleeper, waiter, parker};
        for (Thread t : all) {
            while (t.getState() != Thread.State.TIMED_WAITING && System.currentTimeMillis() < until) {
                Thread.sleep(5);
            }
        }
        Thread.sleep(50);
        if (debugged) {
            phase = 2;
        }
        while (debugged && phase < 3 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        for (Thread t : all) {
            t.interrupt();
            LockSupport.unpark(t);
        }
        for (Thread t : all) {
            t.join(10_000);
        }
        System.out.println("ended");
    }

    static final class Debugger {
        static VirtualMachine vm;
        static final String[] NAMES = {"vsleeper", "vparker", "vwaiter"};

        static String frames(ThreadReference t) throws Exception {
            StringBuilder sb = new StringBuilder();
            for (StackFrame f : t.frames()) {
                if (sb.length() > 0) {
                    sb.append(' ');
                }
                Method m = f.location().method();
                sb.append(m.declaringType().name()).append('.').append(m.name());
                if (m.isNative()) {
                    sb.append("[native]");
                }
                if (m.declaringType().name().equals("L1W48JdiVirtualThreadFrames")) {
                    break;
                }
            }
            return sb.toString();
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
            ClassType type = (ClassType) awaitClass("L1W48JdiVirtualThreadFrames");
            ThreadStartRequest starts = vm.eventRequestManager().createThreadStartRequest();
            starts.setSuspendPolicy(EventRequest.SUSPEND_NONE);
            starts.enable();
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(1));
            Map<String, ThreadReference> seen = new HashMap<>();
            long until = System.currentTimeMillis() + 120_000;
            while (seen.size() < NAMES.length) {
                EventSet set = vm.eventQueue().remove(1_000);
                if (set != null) {
                    for (Event e : set) {
                        if (e instanceof ThreadStartEvent) {
                            ThreadReference t = ((ThreadStartEvent) e).thread();
                            String n = t.name();
                            if (Arrays.asList(NAMES).contains(n)) {
                                seen.put(n, t);
                            }
                        }
                    }
                    set.resume();
                }
                if (System.currentTimeMillis() > until) {
                    fail("the virtual threads never started: " + seen.keySet());
                }
            }
            starts.disable();
            IntegerValue ready;
            do {
                if (System.currentTimeMillis() > until) {
                    fail("the virtual threads never blocked");
                }
                Thread.sleep(50);
                ready = (IntegerValue) type.getValue(type.fieldByName("phase"));
            } while (ready.value() < 2);
            vm.suspend();
            System.out.println("== suspended");
            for (String n : NAMES) {
                ThreadReference t = seen.get(n);
                System.out.println("  " + n + " virtual: " + t.isVirtual());
                System.out.println("  " + n + " frames: " + frames(t));
            }
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(3));
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
