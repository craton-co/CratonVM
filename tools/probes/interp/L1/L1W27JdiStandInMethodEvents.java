// Interpreter round i1 wave 27, lane L1: a JDI scenario for
// docs/internal/fixed-bugs/interpreter-L1-methods-served-without-a-frame-report-no-method-events-FIXED-20261004.md
// (an open page: NOT in tools/jdi/run-jdi-conformance.sh's default list; run
// it with `--scenario L1W27JdiStandInMethodEvents`).
//
//  * MethodEntry / MethodExit requests with SUSPEND_EVENT_THREAD, filtered to
//    the main thread and to the JDK classes whose Java methods CratonVM's
//    interpreter can answer without a frame — from its intrinsic table or a
//    registered native standing in for the Java method (Thread, String,
//    StringBuilder, Integer, Long, Math) — while `callJavaMethods`, whose
//    every call site was linked, and its inline cache filled, before the
//    debugger attached, calls each once: HotSpot reports every one of these
//    Java methods at its first and its returning bytecode index, except
//    `Math.abs(double)` and `Math.sqrt`, which its interpreter serves from
//    math entries that post no event;
//  * a breakpoint at the first location of `String.length()`, filtered to
//    the main thread, while `lengthOf` calls it through a linked call site:
//    the thread stops in `String.length` with `lengthOf` as its caller.
//
// Printed are the events of Java (non-native) methods whose caller (frame 1)
// is the probe's `callJavaMethods`.
//
// Canonical transcript: no ids, no addresses, no timings.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   sum=239
//
// Under a debugger (the debuggee waits up to five minutes for the debugger to
// set the static `attached`):
//
//   javac -g -d out L1W27JdiStandInMethodEvents.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L1W27JdiStandInMethodEvents wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5791 -cp out L1W27JdiStandInMethodEvents wait)
//   java -cp out L1W27JdiStandInMethodEvents debug 5791 > transcript.txt     # HotSpot 25's java
//
// HotSpot 25.0.3 as the debuggee (the debugger's stdout):
//
//   == attach
//   == stop at checkpoint 1
//   == method events of Java methods
//     entry java.lang.Thread.onSpinWait()V index=0
//     exit java.lang.Thread.onSpinWait()V index=0
//     entry java.lang.String.length()I index=0
//     exit java.lang.String.length()I index=10
//     entry java.lang.String.charAt(I)C index=0
//     exit java.lang.String.charAt(I)C index=15
//     entry java.lang.String.isEmpty()Z index=0
//     exit java.lang.String.isEmpty()Z index=13
//     entry java.lang.StringBuilder.setLength(I)V index=0
//     exit java.lang.StringBuilder.setLength(I)V index=5
//     entry java.lang.StringBuilder.append(Ljava/lang/String;)Ljava/lang/StringBuilder; index=0
//     exit java.lang.StringBuilder.append(Ljava/lang/String;)Ljava/lang/StringBuilder; index=7
//     entry java.lang.StringBuilder.append(I)Ljava/lang/StringBuilder; index=0
//     exit java.lang.StringBuilder.append(I)Ljava/lang/StringBuilder; index=7
//     entry java.lang.StringBuilder.append(C)Ljava/lang/StringBuilder; index=0
//     exit java.lang.StringBuilder.append(C)Ljava/lang/StringBuilder; index=7
//     entry java.lang.StringBuilder.append(J)Ljava/lang/StringBuilder; index=0
//     exit java.lang.StringBuilder.append(J)Ljava/lang/StringBuilder; index=7
//     entry java.lang.StringBuilder.append(Z)Ljava/lang/StringBuilder; index=0
//     exit java.lang.StringBuilder.append(Z)Ljava/lang/StringBuilder; index=7
//     entry java.lang.StringBuilder.append(Ljava/lang/Object;)Ljava/lang/StringBuilder; index=0
//     exit java.lang.StringBuilder.append(Ljava/lang/Object;)Ljava/lang/StringBuilder; index=8
//     entry java.lang.StringBuilder.length()I index=0
//     exit java.lang.StringBuilder.length()I index=4
//     entry java.lang.StringBuilder.toString()Ljava/lang/String; index=0
//     exit java.lang.StringBuilder.toString()Ljava/lang/String; index=19
//     entry java.lang.String.length()I index=0
//     exit java.lang.String.length()I index=10
//     entry java.lang.Integer.valueOf(I)Ljava/lang/Integer; index=0
//     exit java.lang.Integer.valueOf(I)Ljava/lang/Integer; index=22
//     entry java.lang.Integer.intValue()I index=0
//     exit java.lang.Integer.intValue()I index=4
//     entry java.lang.Integer.parseInt(Ljava/lang/String;)I index=0
//     exit java.lang.Integer.parseInt(Ljava/lang/String;)I index=6
//     entry java.lang.Long.valueOf(J)Ljava/lang/Long; index=0
//     exit java.lang.Long.valueOf(J)Ljava/lang/Long; index=30
//     entry java.lang.Long.longValue()J index=0
//     exit java.lang.Long.longValue()J index=4
//     entry java.lang.Long.parseLong(Ljava/lang/String;)J index=0
//     exit java.lang.Long.parseLong(Ljava/lang/String;)J index=6
//     entry java.lang.Math.abs(I)I index=0
//     exit java.lang.Math.abs(I)I index=10
//     entry java.lang.Math.abs(J)J index=0
//     exit java.lang.Math.abs(J)J index=12
//     entry java.lang.Math.min(II)I index=0
//     exit java.lang.Math.min(II)I index=10
//     entry java.lang.Math.max(II)I index=0
//     exit java.lang.Math.max(II)I index=10
//     entry java.lang.Math.min(JJ)J index=0
//     exit java.lang.Math.min(JJ)J index=11
//     entry java.lang.Math.max(JJ)J index=0
//     exit java.lang.Math.max(JJ)J index=11
//   == stop at checkpoint 2
//   == breakpoint in String.length
//     breakpoint java.lang.String.length index=0 caller=lengthOf
//   == stop at checkpoint 3
//   == end
//   vm death
//   disconnected
//
// CratonVM at wave 27 (from reading the code; the orchestrator's run is the
// measurement): no line for a method an `Intrinsic` inline-cache entry
// answers (`Thread.onSpinWait`, `String.length` / `charAt` / `isEmpty`,
// `StringBuilder.append` / `length` / `toString`, `Integer.valueOf` /
// `intValue` / `parseInt`, `Long.valueOf` / `longValue` / `parseLong`) or a
// registered native stands in for (the `Math` methods are registered
// `NativeKind::Intrinsic` natives; `StringBuilder.setLength` too, if one is
// registered), and the breakpoint in `String.length` does not stop the
// thread; Math.abs(double) and Math.sqrt are silent as on HotSpot.
//
// Wave 28 (lane L1): expected to match HotSpot in all four modes (not yet
// run on a CratonVM build when written). While a method event request or a
// breakpoint is in force, the native-call funnel runs the bytecode of a Java
// method an intrinsic or a registered native stands in for
// (`jvmti_events::run_stood_in_java_method`), and the `Thread.onSpinWait`
// arm, the one that answers without the funnel, declines to the slow path.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W27JdiStandInMethodEvents {
    static volatile boolean attached;
    static int counter;
    static final String text = "hello";
    static final StringBuilder builder = new StringBuilder();
    static long sink;

    static void checkpoint(int count) {
        counter = count;
    }

    static long callJavaMethods() {
        long sum = 0;
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

    static int lengthOf(String s) {
        return s.length();
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        for (int i = 0; i < 3; i++) {
            sink += callJavaMethods();
            sink += lengthOf(text);
        }
        checkpoint(0);
        boolean debugged = args.length == 1 && args[0].equals("wait");
        if (debugged) {
            waitForDebugger();
        }
        checkpoint(1);
        long sum = callJavaMethods();
        checkpoint(2);
        sum += lengthOf(text);
        checkpoint(3);
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
            "java.lang.Thread", "java.lang.String", "java.lang.StringBuilder",
            "java.lang.Integer", "java.lang.Long", "java.lang.Math",
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
            type = awaitClass("L1W27JdiStandInMethodEvents");
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

            System.out.println("== method events of Java methods");
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

            System.out.println("== breakpoint in String.length");
            ReferenceType string = vm.classesByName("java.lang.String").get(0);
            Method length = null;
            for (Method m : string.methodsByName("length")) {
                if (m.signature().equals("()I")) {
                    length = m;
                }
            }
            BreakpointRequest inLength = erm.createBreakpointRequest(length.location());
            inLength.addThreadFilter(main);
            inLength.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            inLength.enable();
            vm.resume();
            awaitCheckpoint(3);
            erm.deleteEventRequest(inLength);
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
                        Method m = b.location().method();
                        if (m.declaringType().equals(type) && m.name().equals("checkpoint")) {
                            int count = ((IntegerValue) b.thread().frame(0)
                                    .getArgumentValues().get(0)).value();
                            System.out.println("== stop at checkpoint " + count);
                            if (count == n) {
                                stoppedAt = b.thread();
                            }
                        } else {
                            System.out.println("  breakpoint " + m.declaringType().name() + "."
                                    + m.name() + " index=" + b.location().codeIndex()
                                    + " caller=" + b.thread().frame(1).location().method().name());
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
            if (m.isNative() || thread.frameCount() < 2) {
                return;
            }
            Method caller = thread.frame(1).location().method();
            if (!caller.declaringType().equals(type) || !caller.name().equals("callJavaMethods")) {
                return;
            }
            System.out.println("  " + what + " " + m.declaringType().name() + "." + m.name()
                    + m.signature() + " index=" + at.codeIndex());
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
