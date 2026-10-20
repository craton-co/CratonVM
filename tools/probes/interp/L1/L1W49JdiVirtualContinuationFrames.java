// Interpreter round i1 wave 49, lane L1: the frames a debugger lists for
// UNMOUNTED virtual threads blocked in the sleep and park shapes, as HotSpot
// 25 lists them with its default flags (continuations on): the continuation
// rows `VirtualThread.yieldContinuation`, `VirtualThread.parkNanos` / `park`,
// `VirtualThread.sleepNanos`, above the JDK frames that called them
// (docs/internal/fixed-bugs/interpreter-L1-an-unmounted-virtual-thread-lists-its-bound-frames-not-the-continuation-frames-FIXED-20261013.md),
// and which threads `VirtualMachine.AllThreads` lists
// (docs/internal/fixed-bugs/interpreter-L1-allthreads-lists-virtual-threads-FIXED-20261013.md).
//
// The debuggee starts five virtual threads once the debugger has attached
// (the debugger learns them from `ThreadStart` events): `v1sleep` in
// `Thread.sleep(long)`, `v2sleepd` in `Thread.sleep(Duration)`, `v3park` in
// `LockSupport.park()`, `v4parkb` in `LockSupport.parkNanos(Object, long)`,
// `v5parkn` in `LockSupport.parkNanos(long)`. Once all block, the debugger
// suspends the VM and prints each one's frames (method and line), top first,
// down to the probe's own frame, and the probe's threads `allThreads()`
// lists; then it resumes, suspends again and prints the frames once more
// (the first listing runs on a cold VM: nothing resolved or loaded for it).
//
// Canonical transcript: no ids, no addresses, no timings. JDK line numbers
// are printed as the debuggee's JDK has them (the conformance runner
// compares against the host's HotSpot); the transcript below is the
// Windows box's HotSpot 25.0.3.
// Run by the conformance runner, tools/jdi/run-jdi-conformance.sh
// (`--scenario L1W49JdiVirtualContinuationFrames`).
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   ended
//
// Under a debugger:
//
//   javac -g -d out L1W49JdiVirtualContinuationFrames.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L1W49JdiVirtualContinuationFrames wait
//   java -cp out L1W49JdiVirtualContinuationFrames debug 5791 > transcript.txt
//
// HotSpot 25.0.3 as the debuggee (default flags, the Windows box):
//
//   == attach
//   == first
//     v1sleep frames: java.lang.VirtualThread.yieldContinuation:537 java.lang.VirtualThread.parkNanos:784 java.lang.VirtualThread.sleepNanos:980 java.lang.Thread.sleepNanos:507 java.lang.Thread.sleep:540 L1W49JdiVirtualContinuationFrames.lambda$main$0:77
//     v2sleepd frames: java.lang.VirtualThread.yieldContinuation:537 java.lang.VirtualThread.parkNanos:784 java.lang.VirtualThread.sleepNanos:980 java.lang.Thread.sleepNanos:507 java.lang.Thread.sleep:601 L1W49JdiVirtualContinuationFrames.lambda$main$1:84
//     v3park frames: java.lang.VirtualThread.yieldContinuation:537 java.lang.VirtualThread.park:742 java.lang.System$1.parkVirtualThread:2285 java.util.concurrent.locks.LockSupport.park:367 L1W49JdiVirtualContinuationFrames.lambda$main$2:91
//     v4parkb frames: java.lang.VirtualThread.yieldContinuation:537 java.lang.VirtualThread.parkNanos:784 java.lang.System$1.parkVirtualThread:2294 java.util.concurrent.locks.LockSupport.parkNanos:269 L1W49JdiVirtualContinuationFrames.lambda$main$3:95
//     v5parkn frames: java.lang.VirtualThread.yieldContinuation:537 java.lang.VirtualThread.parkNanos:784 java.lang.System$1.parkVirtualThread:2294 java.util.concurrent.locks.LockSupport.parkNanos:406 L1W49JdiVirtualContinuationFrames.lambda$main$4:98
//     allThreads virtual: []
//   == second
//     v1sleep frames: java.lang.VirtualThread.yieldContinuation:537 java.lang.VirtualThread.parkNanos:784 java.lang.VirtualThread.sleepNanos:980 java.lang.Thread.sleepNanos:507 java.lang.Thread.sleep:540 L1W49JdiVirtualContinuationFrames.lambda$main$0:77
//     v2sleepd frames: java.lang.VirtualThread.yieldContinuation:537 java.lang.VirtualThread.parkNanos:784 java.lang.VirtualThread.sleepNanos:980 java.lang.Thread.sleepNanos:507 java.lang.Thread.sleep:601 L1W49JdiVirtualContinuationFrames.lambda$main$1:84
//     v3park frames: java.lang.VirtualThread.yieldContinuation:537 java.lang.VirtualThread.park:742 java.lang.System$1.parkVirtualThread:2285 java.util.concurrent.locks.LockSupport.park:367 L1W49JdiVirtualContinuationFrames.lambda$main$2:91
//     v4parkb frames: java.lang.VirtualThread.yieldContinuation:537 java.lang.VirtualThread.parkNanos:784 java.lang.System$1.parkVirtualThread:2294 java.util.concurrent.locks.LockSupport.parkNanos:269 L1W49JdiVirtualContinuationFrames.lambda$main$3:95
//     v5parkn frames: java.lang.VirtualThread.yieldContinuation:537 java.lang.VirtualThread.parkNanos:784 java.lang.System$1.parkVirtualThread:2294 java.util.concurrent.locks.LockSupport.parkNanos:406 L1W49JdiVirtualContinuationFrames.lambda$main$4:98
//     allThreads virtual: []
//   == end
//     disconnected
//
// CratonVM before wave 49 (both modes) listed the `BoundVirtualThread`
// platform branch that ran (`java.lang.Thread.sleepNanos0[native]`,
// `jdk.internal.misc.Unsafe.park[native]
// java.lang.ThreadBuilders$BoundVirtualThread.parkNanos`, ...: HotSpot's
// rows under `-XX:-VMContinuations`) and listed all five threads in
// `allThreads()`. Positive control: `CRATONVM_FRAME_TRACE=1` prints
// `[VT_CONTINUATION_FRAMES] tid=<n> kind=<sleep|park> kept=<k> levels=<l>`
// for each listing of each thread.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.time.Duration;
import java.util.*;
import java.util.concurrent.locks.LockSupport;

public class L1W49JdiVirtualContinuationFrames {
    /** Set by the debugger: 1 = start the threads, 3 = done. Set to 2 by the debuggee once they block. */
    static volatile int phase;
    static volatile boolean done;
    static final Object BLOCKER = new Object();
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
        Thread[] all = {
            Thread.ofVirtual().name("v1sleep").start(() -> {
                try {
                    Thread.sleep(LONG);
                } catch (InterruptedException stop) {
                    // Ends with the program.
                }
            }),
            Thread.ofVirtual().name("v2sleepd").start(() -> {
                try {
                    Thread.sleep(Duration.ofMillis(LONG));
                } catch (InterruptedException stop) {
                    // Ends with the program.
                }
            }),
            Thread.ofVirtual().name("v3park").start(() -> {
                while (!done) {
                    LockSupport.park();
                }
            }),
            Thread.ofVirtual().name("v4parkb").start(() -> {
                LockSupport.parkNanos(BLOCKER, LONG * 1_000_000L);
            }),
            Thread.ofVirtual().name("v5parkn").start(() -> {
                LockSupport.parkNanos(LONG * 1_000_000L);
            }),
        };
        for (Thread t : all) {
            while (t.getState() != Thread.State.TIMED_WAITING
                    && t.getState() != Thread.State.WAITING
                    && System.currentTimeMillis() < until) {
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
        done = true;
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
        static final String[] NAMES = {"v1sleep", "v2sleepd", "v3park", "v4parkb", "v5parkn"};

        static String frames(ThreadReference t) throws Exception {
            StringBuilder sb = new StringBuilder();
            for (StackFrame f : t.frames()) {
                if (sb.length() > 0) {
                    sb.append(' ');
                }
                Location at = f.location();
                Method m = at.method();
                sb.append(m.declaringType().name()).append('.').append(m.name());
                if (m.isNative()) {
                    sb.append("[native]");
                } else {
                    sb.append(':').append(at.lineNumber());
                }
                if (m.declaringType().name().equals("L1W49JdiVirtualContinuationFrames")) {
                    break;
                }
            }
            return sb.toString();
        }

        static void print(String round, Map<String, ThreadReference> seen) throws Exception {
            System.out.println("== " + round);
            for (String n : NAMES) {
                ThreadReference t = seen.get(n);
                System.out.println("  " + n + " frames: " + frames(t));
            }
            List<String> listed = new ArrayList<>();
            for (ThreadReference t : vm.allThreads()) {
                if (Arrays.asList(NAMES).contains(t.name())) {
                    listed.add(t.name());
                }
            }
            Collections.sort(listed);
            System.out.println("  allThreads virtual: " + listed);
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
            ClassType type = (ClassType) awaitClass("L1W49JdiVirtualContinuationFrames");
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
            print("first", seen);
            vm.resume();
            Thread.sleep(200);
            vm.suspend();
            print("second", seen);
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
