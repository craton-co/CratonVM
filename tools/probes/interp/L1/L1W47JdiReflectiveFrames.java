// Interpreter round i1 wave 47, lane L1: JDWP `ThreadReference.Frames` lists
// a reflective call's JDK frames (`Method.invoke`, the accessor's `invoke`;
// `Constructor.newInstance`, `newInstanceWithCaller`, the accessor's
// `newInstance`), as HotSpot's back end lists them -- item 4 of
// docs/internal/fixed-bugs/interpreter-L3-a-throwable-a-reflective-native-raises-lists-no-reflection-frames-FIXED-20261011.md,
// and stage 1 of
// docs/internal/fixed-bugs/interpreter-i46-L1-proposal-one-frame-listing-for-jdwp-and-the-c-jvmti-table-DONE-20261224.md.
//
// `main` calls `target(int)` through `Method.invoke`, where a breakpoint
// (SUSPEND_EVENT_THREAD) stops it, and constructs `Made` through
// `Constructor.newInstance`, whose constructor holds a second breakpoint. A
// daemon thread `sleeper` sleeps inside `nap()`, itself called through
// `Method.invoke`, and is listed while `main` is stopped at the first
// breakpoint (suspended with the VM: its listing is read through its
// blocked-thread window). At each stop the debugger prints the frames from
// the top down to `main` / `run` (`name:line`, `[native]` for a native
// method). HotSpot also lists the method-handle frames under the accessor
// (`DirectMethodHandle$Holder.invokeStatic`, `Invokers$Holder.invokeExact_MT`
// at -1), which this VM does not run; the probe leaves out every frame of a
// `java.lang.invoke` class, of a hidden class, and `invokeImpl`
// (`L1W46JvmtiReflectiveFrames` leaves them out the same way).
//
// Canonical transcript: no ids, no addresses, no timings.
// Run by the conformance runner, tools/jdi/run-jdi-conformance.sh
// (`--scenario L1W47JdiReflectiveFrames`).
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   target=42 made=7
//
// Under a debugger (the debuggee waits up to five minutes):
//
//   javac -g -d out L1W47JdiReflectiveFrames.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L1W47JdiReflectiveFrames wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5791 -cp out L1W47JdiReflectiveFrames wait)
//   java -cp out L1W47JdiReflectiveFrames debug 5791 > transcript.txt     # HotSpot 25's java
//
// HotSpot 25.0.3 as the debuggee (the Windows box; the orchestrator should
// take the host's HotSpot run as the reference):
//
//   == attach
//   == stop in target
//     main frames: L1W47JdiReflectiveFrames.target:73 jdk.internal.reflect.DirectMethodHandleAccessor.invoke:104 java.lang.reflect.Method.invoke:565 L1W47JdiReflectiveFrames.main:112
//     sleeper frames: java.lang.Thread.sleepNanos0[native] java.lang.Thread.sleepNanos:509 java.lang.Thread.sleep:540 L1W47JdiReflectiveFrames.nap:87 jdk.internal.reflect.DirectMethodHandleAccessor.invoke:104 java.lang.reflect.Method.invoke:565 L1W47JdiReflectiveFrames.lambda$main$0:97
//   == stop in init
//     main frames: L1W47JdiReflectiveFrames$Made.<init>:80 jdk.internal.reflect.DirectConstructorHandleAccessor.newInstance:62 java.lang.reflect.Constructor.newInstanceWithCaller:499 java.lang.reflect.Constructor.newInstance:483 L1W47JdiReflectiveFrames.main:113
//   == end
//     disconnected
//
// CratonVM before wave 47 (from the code): `Method.invoke` and
// `Constructor.newInstance` are served by natives, and the JDWP listing had
// no row for their JDK frames, so `target` sat directly on `main`
// (`main frames: L1W47JdiReflectiveFrames.target:... L1W47JdiReflectiveFrames.main:...`).
// Now the listing splices the frames the stack trace lists
// (`interpreter::build_frame_listing`, from the thread's memo
// `JvmThread::reflective_frames_named`), switched by
// `CRATONVM_DEBUG_ONE_FRAME_LISTING` (default on; `=0` restores the base).
// Positive control: `CRATONVM_FRAME_TRACE=1` prints `[DEBUG_LISTING] tid=<n>
// splices=1 rows=2` for each listing with a reflective call.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W47JdiReflectiveFrames {
    /** 0 until the debugger has set its breakpoints. */
    static volatile int go;
    static volatile int napping;

    static int target(int x) {
        int y = x * 2;
        return y;
    }

    static final class Made {
        final int v;

        Made(int v) {
            this.v = v;
        }
    }

    static void nap() throws InterruptedException {
        napping = 1;
        Thread.sleep(600_000);
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        Thread sleeper = new Thread(() -> {
            try {
                L1W47JdiReflectiveFrames.class.getDeclaredMethod("nap").invoke(null);
            } catch (ReflectiveOperationException stop) {
                // Ends with the program.
            }
        }, "sleeper");
        sleeper.setDaemon(true);
        sleeper.start();
        // Loaded, so the debugger finds it by name.
        Class.forName(Made.class.getName());
        if (args.length == 1 && args[0].equals("wait")) {
            long until = System.currentTimeMillis() + 300_000;
            while ((go == 0 || napping == 0) && System.currentTimeMillis() < until) {
                Thread.sleep(10);
            }
        }
        Object t = L1W47JdiReflectiveFrames.class.getDeclaredMethod("target", int.class).invoke(null, 21);
        Made m = Made.class.getDeclaredConstructor(int.class).newInstance(7);
        sleeper.interrupt();
        System.out.println("target=" + t + " made=" + m.v);
    }

    static final class Debugger {
        static VirtualMachine vm;

        static String frames(ThreadReference t) throws Exception {
            StringBuilder sb = new StringBuilder();
            for (StackFrame f : t.frames()) {
                Location l = f.location();
                Method m = l.method();
                String type = m.declaringType().name();
                if (type.startsWith("java.lang.invoke.") || type.contains("/0x")
                        || m.name().equals("invokeImpl")) {
                    continue;
                }
                if (sb.length() > 0) {
                    sb.append(' ');
                }
                sb.append(type).append('.').append(m.name());
                if (m.isNative()) {
                    sb.append("[native]");
                } else {
                    sb.append(':').append(l.lineNumber());
                }
                if (type.equals("L1W47JdiReflectiveFrames")
                        && (m.name().equals("main") || m.name().startsWith("lambda$"))) {
                    break;
                }
            }
            return sb.toString();
        }

        static ThreadReference thread(String name) {
            for (ThreadReference t : vm.allThreads()) {
                if (t.name().equals(name)) {
                    return t;
                }
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
            EventRequestManager erm = vm.eventRequestManager();
            System.out.println("== attach");
            ClassType type = (ClassType) awaitClass("L1W47JdiReflectiveFrames");
            ReferenceType made = awaitClass("L1W47JdiReflectiveFrames$Made");
            BreakpointRequest inTarget = erm.createBreakpointRequest(
                    type.methodsByName("target").get(0).location());
            inTarget.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            inTarget.putProperty("name", "target");
            inTarget.enable();
            BreakpointRequest inInit = erm.createBreakpointRequest(
                    made.methodsByName("<init>").get(0).location());
            inInit.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            inInit.putProperty("name", "init");
            inInit.enable();
            type.setValue(type.fieldByName("go"), vm.mirrorOf(1));
            boolean ended = false;
            int stops = 0;
            long until = System.currentTimeMillis() + 120_000;
            while (!ended && stops < 2 && System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(5_000);
                if (set == null) {
                    continue;
                }
                for (Event e : set) {
                    if (e instanceof BreakpointEvent b) {
                        stops++;
                        String n = (String) b.request().getProperty("name");
                        System.out.println("== stop in " + n);
                        System.out.println("  main frames: " + frames(b.thread()));
                        if (n.equals("target")) {
                            ThreadReference sleeper = thread("sleeper");
                            sleeper.suspend();
                            System.out.println("  sleeper frames: " + frames(sleeper));
                            sleeper.resume();
                        }
                    } else if (e instanceof VMDeathEvent || e instanceof VMDisconnectEvent) {
                        ended = true;
                    }
                }
                if (!ended) {
                    set.resume();
                }
            }
            if (stops < 2) {
                fail("stopped " + stops + " times");
            }
            System.out.println("== end");
            erm.deleteAllBreakpoints();
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
