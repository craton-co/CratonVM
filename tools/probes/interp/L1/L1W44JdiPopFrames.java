// Interpreter round i1 wave 44, lane L1: JDWP StackFrame.PopFrames (16/4),
// stage 3 of
// docs/internal/fixed-bugs/interpreter-L1-proposal-pop-frames-force-early-return-and-source-debug-extension-FIXED-20261008.md
// (IntelliJ's and Eclipse's "Drop Frame").
//
// The debuggee waits (`go`) until the debugger has set a breakpoint in
// `depth2` (inside a `synchronized (LOCK)` block), then calls `depth1(5)`,
// which calls `depth2(a)`. At the first hit the debugger writes `x = 7` into
// `depth2`'s frame and pops that one frame: the thread is then at `depth1`'s
// invoke of `depth2`, `LOCK` is free again, and resuming re-runs the call
// with the changed argument (JDI: "any changes to the arguments that
// occurred in the called method, remain"). At the second hit it pops two
// frames at once (`depth1` too), so `main` re-runs `depth1(5)`; the third hit
// is resumed. Popping `main`'s frame, which has no Java caller, is refused.
//
// Canonical transcript: no ids, no addresses, no timings.
// Run by the conformance runner, tools/jdi/run-jdi-conformance.sh
// (`--scenario L1W44JdiPopFrames`).
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   r=51 calls=1
//
// Under a debugger (the debuggee waits up to five minutes):
//
//   javac -g -d out L1W44JdiPopFrames.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L1W44JdiPopFrames wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5791 -cp out L1W44JdiPopFrames wait)
//   java -cp out L1W44JdiPopFrames debug 5791 > transcript.txt     # HotSpot 25's java
//
// HotSpot 25.0.3 as the debuggee (the debugger's stdout; three runs on the
// Windows box, the same each time; the debuggee then prints `r=51 calls=3`).
// Lines are relative to `depth2`'s first line:
//
//   == attach
//     canPopFrames: true
//   == hit 1
//     where: depth2:3 depth1:9 main:25
//     locals: x=5 y=50
//     monitors owned: 1 (LOCK)
//     pop depth2: popped
//     where: depth1:9 main:25
//     caller locals: a=5
//     monitors owned: 0
//     frame count: 2
//   == hit 2
//     where: depth2:3 depth1:9 main:25
//     locals: x=7 y=70
//     monitors owned: 1 (LOCK)
//     pop depth2 and depth1: popped
//     where: main:25
//     monitors owned: 0
//     pop main: InvalidStackFrameException
//     where: main:25
//   == hit 3
//     where: depth2:3 depth1:9 main:25
//     locals: x=5 y=50
//     monitors owned: 1 (LOCK)
//   == end
//     disconnected
//
// CratonVM before wave 44 (from reading the code): `canPopFrames: false`,
// and JDI throws `UnsupportedOperationException` from `popFrames` itself,
// so both `pop` rows of hit 1 and 2 print it, nothing is popped, and there
// is no third hit (the transcript differs from `canPopFrames:` on). Since
// wave 44 the pop runs on the parked thread (`debug::pop_frames`). Positive
// control: `CRATONVM_FRAME_TRACE=1` prints
// `[POP_FRAMES] tid=<n> popped=1 top=L1W44JdiPopFrames.depth1(I)I at=<invoke bci> args=1`
// and `[POP_FRAMES] tid=<n> popped=2 top=L1W44JdiPopFrames.main([Ljava/lang/String;)V at=<invoke bci> args=1`,
// and `[POP_FRAMES] refused tid=<n> error=31` for `main`'s frame. A
// `[POP_FRAMES] caller of another loop` line means the frames were not
// pushed by one dispatch loop (the pop is then refused as OPAQUE_FRAME,
// which JDI throws as `NativeMethodException`).
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W44JdiPopFrames {
    /** 0 until the debugger has set its breakpoint. */
    static volatile int go;
    static final Object LOCK = new Object();
    static int calls;

    static int depth2(int x) {
        calls++;
        int y = x * 10;
        synchronized (LOCK) {
            y = y + 1; // BREAKPOINT_LINE
        }
        return y;
    }

    static int depth1(int a) {
        int r = depth2(a);
        return r;
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        if (args.length == 1 && args[0].equals("wait")) {
            long until = System.currentTimeMillis() + 300_000;
            while (go == 0 && System.currentTimeMillis() < until) {
                Thread.sleep(10);
            }
        }
        calls = 0;
        int r = depth1(5);
        System.out.println("r=" + r + " calls=" + calls);
    }

    static final class Debugger {
        static VirtualMachine vm;

        static String where(ThreadReference t) throws Exception {
            StringBuilder sb = new StringBuilder();
            for (StackFrame f : t.frames()) {
                if (sb.length() > 0) {
                    sb.append(' ');
                }
                Location l = f.location();
                sb.append(l.method().name()).append(':').append(l.lineNumber() - BASE_LINE);
            }
            return sb.toString();
        }

        /** The line numbers are printed relative to this class's `depth2` line. */
        static int BASE_LINE;

        static String locals(StackFrame f) throws Exception {
            StringBuilder sb = new StringBuilder();
            for (LocalVariable v : f.visibleVariables()) {
                if (sb.length() > 0) {
                    sb.append(' ');
                }
                sb.append(v.name()).append('=').append(f.getValue(v));
            }
            return sb.toString();
        }

        /** `LOCK` among the monitors `t` owns, and how many it owns. */
        static String owned(ThreadReference t, ObjectReference lock) throws Exception {
            List<ObjectReference> owned = t.ownedMonitors();
            return owned.size() + (owned.contains(lock) ? " (LOCK)" : "");
        }

        static String pop(ThreadReference t, int index) {
            try {
                t.popFrames(t.frame(index));
                return "popped";
            } catch (Exception e) {
                return e.getClass().getSimpleName();
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
            EventRequestManager erm = vm.eventRequestManager();
            System.out.println("== attach");
            System.out.println("  canPopFrames: " + vm.canPopFrames());
            ReferenceType type = awaitClass("L1W44JdiPopFrames");
            ThreadReference main = null;
            for (ThreadReference t : vm.allThreads()) {
                if (t.name().equals("main")) {
                    main = t;
                }
            }
            if (main == null) {
                fail("no main thread");
            }
            Method depth2 = type.methodsByName("depth2").get(0);
            TreeSet<Integer> lines = new TreeSet<>();
            for (Location l : depth2.allLineLocations()) {
                lines.add(l.lineNumber());
            }
            BASE_LINE = lines.first();
            // `y = y + 1;`: the fourth distinct line of `depth2`.
            int bpLine = new ArrayList<>(lines).get(3);
            ObjectReference lock = (ObjectReference) type.getValue(type.fieldByName("LOCK"));

            BreakpointRequest bp = erm.createBreakpointRequest(depth2.locationsOfLine(bpLine).get(0));
            bp.addThreadFilter(main);
            bp.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            bp.enable();
            ((ClassType) type).setValue(type.fieldByName("go"), vm.mirrorOf(1));
            int hit = 0;
            boolean ended = false;
            long until = System.currentTimeMillis() + 120_000;
            while (!ended && System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(2_000);
                if (set == null) {
                    continue;
                }
                for (Event e : set) {
                    if (e instanceof BreakpointEvent b) {
                        hit++;
                        ThreadReference t = b.thread();
                        System.out.println("== hit " + hit);
                        System.out.println("  where: " + where(t));
                        System.out.println("  locals: " + locals(t.frame(0)));
                        System.out.println("  monitors owned: " + owned(t, lock));
                        if (hit == 1) {
                            StackFrame f0 = t.frame(0);
                            f0.setValue(f0.visibleVariableByName("x"), vm.mirrorOf(7));
                            System.out.println("  pop depth2: " + pop(t, 0));
                            System.out.println("  where: " + where(t));
                            System.out.println("  caller locals: " + locals(t.frame(0)));
                            System.out.println("  monitors owned: " + owned(t, lock));
                            System.out.println("  frame count: " + t.frameCount());
                        } else if (hit == 2) {
                            System.out.println("  pop depth2 and depth1: " + pop(t, 1));
                            System.out.println("  where: " + where(t));
                            System.out.println("  monitors owned: " + owned(t, lock));
                            System.out.println("  pop main: " + pop(t, t.frameCount() - 1));
                            System.out.println("  where: " + where(t));
                        } else {
                            erm.deleteEventRequest(bp);
                        }
                    } else if (e instanceof VMDeathEvent || e instanceof VMDisconnectEvent) {
                        ended = true;
                    }
                }
                if (!ended) {
                    set.resume();
                }
                if (hit >= 3) {
                    break;
                }
            }
            System.out.println("== end");
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
