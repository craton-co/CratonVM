// Interpreter round i1 wave 27, lane L1: the JDI conformance harness's
// `intrinsic method events` scenario, for
// docs/internal/fixed-bugs/interpreter-L1-jdwp-method-events-and-stop-miss-native-and-compiled-code-FIXED-20261005.md
// ("What remains (after wave 26)", item 1) and stage 5 of the proposal
// interpreter-L1-proposal-native-method-events-at-the-native-funnel-FIXED-20260928.md:
//
//  * MethodEntry / MethodExit requests with SUSPEND_EVENT_THREAD, filtered to
//    the main thread and to the JDK classes whose methods CratonVM's
//    interpreter serves from its intrinsic table (Object, System, Thread,
//    String, StringBuilder, Integer, Long, Math), while `callIntrinsics` —
//    whose every call site was linked, and its inline cache filled, before the
//    debugger attached — calls each table method once, and
//    `System.identityHashCode`, a native outside the table: every native
//    method HotSpot reports is reported, in call order, at location -1, with
//    `callIntrinsics` as its caller (frame 1). HotSpot reports `getClass`,
//    `hashCode`, `identityHashCode` and `arraycopy`, and not
//    `Thread.currentThread`, whose interpreter entry posts no event.
//
// Printed are the events of native methods whose caller (frame 1) is the
// probe's `callIntrinsics`. The Java methods of the same calls (String.length,
// StringBuilder.append, Integer.valueOf, Math.max, ...) are the subject of
// L1W27JdiStandInMethodEvents, which is not in the runner's default list
// (docs/internal/fixed-bugs/interpreter-L1-methods-served-without-a-frame-report-no-method-events-FIXED-20261004.md).
//
// Canonical transcript: no ids, no addresses, no timings.
// Run by the conformance runner, tools/jdi/run-jdi-conformance.sh
// (`--scenario L1W27JdiIntrinsicMethodEvents`).
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   sum=240
//
// Under a debugger (the debuggee waits up to five minutes for the debugger to
// set the static `attached`):
//
//   javac -g -d out L1W27JdiIntrinsicMethodEvents.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L1W27JdiIntrinsicMethodEvents wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5791 -cp out L1W27JdiIntrinsicMethodEvents wait)
//   java -cp out L1W27JdiIntrinsicMethodEvents debug 5791 > transcript.txt     # HotSpot 25's java
//
// HotSpot 25.0.3 as the debuggee (the debugger's stdout):
//
//   == attach
//   == stop at checkpoint 1
//   == method events of the intrinsic table's classes
//     entry java.lang.Object.getClass()Ljava/lang/Class; native index=-1
//     exit java.lang.Object.getClass()Ljava/lang/Class; native index=-1
//     entry java.lang.Object.hashCode()I native index=-1
//     exit java.lang.Object.hashCode()I native index=-1
//     entry java.lang.System.identityHashCode(Ljava/lang/Object;)I native index=-1
//     exit java.lang.System.identityHashCode(Ljava/lang/Object;)I native index=-1
//     entry java.lang.System.arraycopy(Ljava/lang/Object;ILjava/lang/Object;II)V native index=-1
//     exit java.lang.System.arraycopy(Ljava/lang/Object;ILjava/lang/Object;II)V native index=-1
//   == stop at checkpoint 2
//   == end
//   vm death
//   disconnected
//
// CratonVM before wave 27 (from reading the code; the orchestrator's run is
// the measurement): only the `identityHashCode` lines. `getClass`, `hashCode`
// and `arraycopy` reach the native-call funnel from an `Intrinsic` inline
// cache entry, which runs the intrinsic's trampoline instead of the method's
// registered callback, and `native_named_by_invoke` confirmed the method by
// the registered callback only.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W27JdiIntrinsicMethodEvents {
    static volatile boolean attached;
    static int counter;
    static final Object plain = new Object();
    static final String text = "hello";
    static final StringBuilder builder = new StringBuilder();
    static final int[] source = {1, 2, 3};
    static final int[] target = new int[3];
    static long sink;

    static void checkpoint(int count) {
        counter = count;
    }

    static long callIntrinsics() {
        long sum = 0;
        sum += plain.getClass() == Object.class ? 1 : 0;
        sum += plain.hashCode() == System.identityHashCode(plain) ? 1 : 0;
        System.arraycopy(source, 0, target, 0, 3);
        sum += target[2];
        sum += Thread.currentThread() != null ? 1 : 0;
        Thread.onSpinWait();
        sum += text.length();
        sum += text.charAt(1);
        sum += text.isEmpty() ? 1 : 0;
        builder.setLength(0);
        builder.append("ab");
        builder.append(7);
        builder.append('c');
        builder.append(8L);
        builder.append(true);
        builder.append((Object) text);
        sum += builder.length();
        sum += builder.toString().length();
        Integer boxed = Integer.valueOf(7);
        sum += boxed.intValue();
        sum += Integer.parseInt("42");
        Long wide = Long.valueOf(9L);
        sum += wide.longValue();
        sum += Long.parseLong("11");
        sum += Math.abs(-3);
        sum += Math.abs(-4L);
        sum += (long) Math.abs(-2.5);
        sum += Math.min(3, 4);
        sum += Math.max(3, 4);
        sum += Math.min(5L, 6L);
        sum += Math.max(5L, 6L);
        sum += (long) Math.sqrt(16.0);
        return sum;
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        for (int i = 0; i < 3; i++) {
            sink += callIntrinsics();
        }
        checkpoint(0);
        boolean debugged = args.length == 1 && args[0].equals("wait");
        if (debugged) {
            waitForDebugger();
        }
        checkpoint(1);
        long sum = callIntrinsics();
        checkpoint(2);
        System.out.println("sum=" + sum);
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
        static final String[] CLASSES = {
            "java.lang.Object", "java.lang.System", "java.lang.Thread", "java.lang.String",
            "java.lang.StringBuilder", "java.lang.Integer", "java.lang.Long", "java.lang.Math",
        };
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
            type = awaitClass("L1W27JdiIntrinsicMethodEvents");
            BreakpointRequest bp = erm.createBreakpointRequest(
                    type.methodsByName("checkpoint").get(0).location());
            bp.setSuspendPolicy(EventRequest.SUSPEND_ALL);
            bp.enable();
            // The (no-op) resume first: sent after the flag, it could reach a
            // slow back end after the program had run into its first
            // SUSPEND_ALL stop, and release that stop.
            vm.resume();
            ((ClassType) type).setValue(type.fieldByName("attached"), vm.mirrorOf(true));
            ThreadReference main = awaitCheckpoint(1);

            System.out.println("== method events of the intrinsic table's classes");
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

        /** Resume every event set until the debuggee's death and disconnection. */
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

        /** Print the events until checkpoint `n`'s breakpoint. */
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
                        print("entry", me.method(), me.location(), me.thread());
                    } else if (e instanceof MethodExitEvent mx) {
                        print("exit", mx.method(), mx.location(), mx.thread());
                    } else if (e instanceof BreakpointEvent b) {
                        int count = ((IntegerValue) b.thread().frame(0)
                                .getArgumentValues().get(0)).value();
                        System.out.println("== stop at checkpoint " + count);
                        if (count == n) {
                            stoppedAt = b.thread();
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

        static void print(String what, Method m, Location at, ThreadReference thread)
                throws Exception {
            if (!m.isNative() || thread.frameCount() < 2) {
                return;
            }
            Method caller = thread.frame(1).location().method();
            if (!caller.declaringType().equals(type) || !caller.name().equals("callIntrinsics")) {
                return;
            }
            System.out.println("  " + what + " " + m.declaringType().name() + "." + m.name()
                    + m.signature() + " native index=" + at.codeIndex());
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
