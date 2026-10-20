// Interpreter round i1 wave 29, lane L1: a JDI scenario for item 1 of
// docs/internal/fixed-bugs/interpreter-L1-jdwp-method-events-and-stop-miss-native-and-compiled-code-FIXED-20261005.md
// (a native reached through reflection or a method handle). Not in
// tools/jdi/run-jdi-conformance.sh's default list until a host run shows it
// matches; run it with `--scenario L1W29JdiReflectedNativeMethodEvents`.
//
// Wave 29 host run: every mode matched HotSpot but for the two
// `Runtime.availableProcessors()` rows (an instance native through
// `Method.invoke`, which reaches `NativeContext::invoke_virtual` →
// `invoke_or_native`, whose registry arms did not name the native to the
// funnel). Wave 40 (lane L1): those arms name it
// (`vm_exec::by_name_native_call_held`); `CRATONVM_FRAME_TRACE=1` prints a
// `[HELD_NATIVE] class=<id> method_id=<id>` line for each of the two rows.
//
//  * MethodEntry / MethodExit (with its return value) requests with
//    SUSPEND_EVENT_THREAD, filtered to the main thread and to Float, Double
//    and Runtime, while `callNativesIndirectly` calls four genuine natives
//    ONLY through `Method.invoke` (a static and an instance native) and
//    through `MethodHandle.invokeExact` / `invoke` (two static natives):
//    HotSpot reports every native's entry and exit at location -1, with the
//    return value. Every call is made before the debugger attaches, so each
//    reflective accessor and method handle is linked.
//
// Printed are the events of native methods only (the reflection and
// method-handle machinery's Java methods are filtered out).
//
// Canonical transcript: no ids, no addresses, no timings, no processor count.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   bits=1069547520 back=1.5 raw=4609434218613702656 processors>0=true
//
// Under a debugger (the debuggee waits up to five minutes for the debugger to
// set the static `attached`):
//
//   javac -g -d out L1W29JdiReflectedNativeMethodEvents.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L1W29JdiReflectedNativeMethodEvents wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5791 -cp out L1W29JdiReflectedNativeMethodEvents wait)
//   java -cp out L1W29JdiReflectedNativeMethodEvents debug 5791 > transcript.txt     # HotSpot 25's java
//
// HotSpot 25.0.3 as the debuggee (the debugger's stdout):
//
//   == attach
//   == stop at checkpoint 1
//   == natives reached through reflection and method handles
//     entry java.lang.Float.floatToRawIntBits(F)I index=-1
//     exit java.lang.Float.floatToRawIntBits(F)I index=-1 returns 1069547520
//     entry java.lang.Float.intBitsToFloat(I)F index=-1
//     exit java.lang.Float.intBitsToFloat(I)F index=-1 returns 1.5
//     entry java.lang.Double.doubleToRawLongBits(D)J index=-1
//     exit java.lang.Double.doubleToRawLongBits(D)J index=-1 returns 4609434218613702656
//     entry java.lang.Runtime.availableProcessors()I index=-1
//     exit java.lang.Runtime.availableProcessors()I index=-1 returns positive=true
//   == stop at checkpoint 2
//   == end
//   vm death
//   disconnected
//
// CratonVM before wave 29 (from reading the code): no line between the two
// checkpoints. The native-call funnel names the native it runs from the top
// interpreter frame's invoke (`interpreter::native_named_by_invoke`), which
// here is the reflection or method-handle machinery's own call
// (`MethodHandle.linkToStatic`, `Method.invoke`, ...), never the native, so
// it reported nothing. Wave 29: the general invocation door that holds the
// method (`vm_exec::invoke_on_class_shared_inner`'s registry arm) names it to
// the funnel (`jvmti_events::with_held_native`).
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.*;

public class L1W29JdiReflectedNativeMethodEvents {
    static volatile boolean attached;
    static int counter;
    static String line;

    static final java.lang.reflect.Method FLOAT_TO_BITS;
    static final java.lang.reflect.Method PROCESSORS;
    static final MethodHandle BITS_TO_FLOAT;
    static final MethodHandle DOUBLE_TO_BITS;

    static {
        try {
            FLOAT_TO_BITS = Float.class.getMethod("floatToRawIntBits", float.class);
            PROCESSORS = Runtime.class.getMethod("availableProcessors");
            MethodHandles.Lookup lookup = MethodHandles.lookup();
            BITS_TO_FLOAT = lookup.findStatic(Float.class, "intBitsToFloat",
                    MethodType.methodType(float.class, int.class));
            DOUBLE_TO_BITS = lookup.findStatic(Double.class, "doubleToRawLongBits",
                    MethodType.methodType(long.class, double.class));
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static void checkpoint(int count) {
        counter = count;
    }

    static int bits;
    static float back;
    static long raw;
    static int processors;

    // No formatting in here: `Float.toString` reaches natives of its own.
    static void callNativesIndirectly() throws Throwable {
        bits = (Integer) FLOAT_TO_BITS.invoke(null, 1.5f);
        back = (float) BITS_TO_FLOAT.invokeExact(bits);
        raw = (long) DOUBLE_TO_BITS.invokeExact(1.5);
        processors = (Integer) PROCESSORS.invoke(Runtime.getRuntime());
    }

    static String line() {
        return "bits=" + bits + " back=" + back + " raw=" + raw
                + " processors>0=" + (processors > 0);
    }

    public static void main(String[] args) throws Throwable {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        for (int i = 0; i < 3; i++) {
            callNativesIndirectly();
            line = line();
        }
        checkpoint(0);
        boolean debugged = args.length == 1 && args[0].equals("wait");
        if (debugged) {
            waitForDebugger();
        }
        checkpoint(1);
        callNativesIndirectly();
        checkpoint(2);
        System.out.println(line());
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
        static final String[] CLASSES = {"java.lang.Float", "java.lang.Double", "java.lang.Runtime"};
        static VirtualMachine vm;
        static ReferenceType type;

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
            type = awaitClass("L1W29JdiReflectedNativeMethodEvents");
            BreakpointRequest bp = erm.createBreakpointRequest(
                    type.methodsByName("checkpoint").get(0).location());
            bp.setSuspendPolicy(EventRequest.SUSPEND_ALL);
            bp.enable();
            vm.resume();
            ((ClassType) type).setValue(type.fieldByName("attached"), vm.mirrorOf(true));
            ThreadReference main = awaitCheckpoint(1);

            System.out.println("== natives reached through reflection and method handles");
            List<EventRequest> events = new ArrayList<>();
            for (String name : CLASSES) {
                MethodEntryRequest entry = erm.createMethodEntryRequest();
                entry.addThreadFilter(main);
                entry.addClassFilter(name);
                events.add(entry);
                MethodExitRequest exit = erm.createMethodExitRequest();
                exit.addThreadFilter(main);
                exit.addClassFilter(name);
                events.add(exit);
            }
            for (EventRequest r : events) {
                r.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
                r.enable();
            }
            vm.resume();
            awaitCheckpoint(2);
            for (EventRequest r : events) {
                erm.deleteEventRequest(r);
            }
            System.out.println("== end");
            erm.deleteAllBreakpoints();
            vm.resume();
            awaitDeath();
        }

        static void awaitDeath() throws Exception {
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

        static ThreadReference awaitCheckpoint(int n) throws Exception {
            long until = System.currentTimeMillis() + 120_000;
            while (System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(500);
                if (set == null) {
                    continue;
                }
                ThreadReference stoppedAt = null;
                for (Event e : set) {
                    if (e instanceof MethodEntryEvent me) {
                        if (me.method().isNative()) {
                            System.out.println("  entry " + name(me.method())
                                    + " index=" + me.location().codeIndex());
                        }
                    } else if (e instanceof MethodExitEvent mx) {
                        if (mx.method().isNative()) {
                            System.out.println("  exit " + name(mx.method())
                                    + " index=" + mx.location().codeIndex()
                                    + " returns " + show(mx.method(), mx.returnValue()));
                        }
                    } else if (e instanceof BreakpointEvent b) {
                        Method m = b.location().method();
                        if (m.declaringType().equals(type) && m.name().equals("checkpoint")) {
                            int count = ((IntegerValue) b.thread().frame(0)
                                    .getArgumentValues().get(0)).value();
                            System.out.println("== stop at checkpoint " + count);
                            if (count == n) {
                                stoppedAt = b.thread();
                            }
                        }
                    } else if (e instanceof VMDeathEvent || e instanceof VMDisconnectEvent) {
                        fail("the debuggee ended early");
                    }
                }
                if (stoppedAt != null) {
                    return stoppedAt;
                }
                set.resume();
            }
            fail("no checkpoint " + n);
            return null;
        }

        static String name(Method m) {
            return m.declaringType().name() + "." + m.name() + m.signature();
        }

        static String show(Method m, Value v) {
            if (m.name().equals("availableProcessors")) {
                return "positive=" + (v instanceof IntegerValue iv && iv.value() > 0);
            }
            return String.valueOf(v);
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
