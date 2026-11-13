// Interpreter round i7 wave 137, lane L1: a JDI STEP_OUT (STEP_LINE) from a
// method reached through an indirect call -- `Method.invoke`, a
// `MethodHandle.invokeExact`, a lambda's `IntSupplier.getAsInt` -- with and
// without the step filters every IDE sets (`java.*`, `jdk.*`, `sun.*`).
//
// The debuggee waits (`go`) until the debugger has set its breakpoints, then
// calls `viaReflection`, `viaHandle` and `viaLambda` from `work`, each
// calling its target once with the debugger's step as `out filtered`, then
// once more with the bare step (`out bare`). At each target's breakpoint the
// debugger makes a step OUT (LINE) on the thread (SUSPEND_EVENT_THREAD),
// prints where it completes and deletes it. A hidden class's name is cut at
// its `/` (the address suffix); lines are printed as JDI answers them (-1
// for none).
//
// What HotSpot shows (MEASURED): the filtered step out of the reflective and
// the method-handle target completes in `work` on the line of the call; the
// bare one in `DirectMethodHandle$Holder.invokeStatic` (no line), a frame
// CratonVM does not run (`Method.invoke` and `invokeExact` are served without
// the method-handle frames; the listing splices the reflective rows only,
// `L1W47JdiReflectiveFrames`; the known gap
// docs/feature-designs/class-resolution-and-caching.md).
// The step out of the lambda target completes in the spun proxy's
// `getAsInt` (no line), filtered or not: a hidden `...$$Lambda` class is
// no `java.*` class. CratonVM lists that proxy method as a row
// (`CRATONVM_JVMTI_C_LAMBDA_FRAMES`, the i54-L1 retirement) but no step
// completes there (predicted from the code: `work:9` for both lambda rows),
// docs/feature-designs/jvmti-delivery-threading.md.
// The `reflected out filtered` and `handled out filtered` rows must match.
//
// Canonical transcript: no ids, no addresses, no timings. Lines are relative
// to `work`'s first line for `work`, absolute elsewhere.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   sum=52
//
// Under a debugger (the debuggee waits up to five minutes):
//
//   javac -g -d out L1W137JdiStepOutOfIndirectCall.java
//   cratonvm --java-home $JDK --jdwp-port 5797 -cp out L1W137JdiStepOutOfIndirectCall wait
//   #   (HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5797 -cp out L1W137JdiStepOutOfIndirectCall wait)
//   java -cp out L1W137JdiStepOutOfIndirectCall debug 5797 > transcript.txt     # HotSpot 25's java
//   # both modes (--compatible too), default and --nojit
//
// HotSpot 25.0.3 as the debuggee: see HOTSPOT_EXPECTED below (debugger
// transcript; MEASURED twice on the Windows box, identical).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, Windows, default flags)
// == attach
//   reflected out filtered: Main.work:7
//   handled out filtered: Main.work:8
//   lambdaTarget out filtered: $Lambda.getAsInt:-1
//   reflected out bare: java.lang.invoke.DirectMethodHandle$Holder.invokeStatic:-1
//   handled out bare: java.lang.invoke.DirectMethodHandle$Holder.invokeStatic:-1
//   lambdaTarget out bare: $Lambda.getAsInt:-1
// == end
//   disconnected
// HOTSPOT_EXPECTED_END
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.*;
import java.util.function.IntSupplier;

public class L1W137JdiStepOutOfIndirectCall {
    /** 0 until the debugger has set its breakpoints. */
    static volatile int go;

    static int reflected(int x) {
        int y = x + 1;
        return y;
    }

    static int handled(int x) {
        int y = x + 2;
        return y;
    }

    static int lambdaTarget() {
        int y = 3;
        return y;
    }

    static int work() throws Throwable {
        java.lang.reflect.Method m = L1W137JdiStepOutOfIndirectCall.class
                .getDeclaredMethod("reflected", int.class); // work's first line
        MethodHandle h = MethodHandles.lookup().findStatic(L1W137JdiStepOutOfIndirectCall.class,
                "handled", MethodType.methodType(int.class, int.class));
        IntSupplier s = L1W137JdiStepOutOfIndirectCall::lambdaTarget;
        int sum = 0;
        for (int round = 0; round < 2; round++) {
            sum += (Integer) m.invoke(null, 10);
            sum += (int) h.invokeExact(10);
            sum += s.getAsInt();
        }
        return sum;
    }

    public static void main(String[] args) throws Throwable {
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
        System.out.println("sum=" + work());
    }

    static final class Debugger {
        static final String MAIN = "L1W137JdiStepOutOfIndirectCall";
        static VirtualMachine vm;
        static int base;

        static String at(Location l) {
            String type = l.declaringType().name();
            int slash = type.indexOf('/');
            if (slash >= 0) {
                type = type.substring(0, slash);
            }
            type = type.replace(MAIN + "$", "").replace(MAIN, "Main");
            int line = l.lineNumber();
            String shown = l.method().name().equals("work") && line > 0
                    ? Integer.toString(line - base)
                    : Integer.toString(line);
            return type + "." + l.method().name() + ":" + shown;
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
            ClassType main = (ClassType) awaitClass(MAIN);
            TreeSet<Integer> lines = new TreeSet<>();
            for (Location l : main.methodsByName("work").get(0).allLineLocations()) {
                lines.add(l.lineNumber());
            }
            base = lines.first();
            Map<EventRequest, String> names = new HashMap<>();
            for (String target : new String[] {"reflected", "handled", "lambdaTarget"}) {
                Location first = main.methodsByName(target).get(0).allLineLocations().get(0);
                BreakpointRequest bp = erm.createBreakpointRequest(first);
                bp.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
                names.put(bp, target);
                bp.enable();
            }
            Map<String, Integer> hits = new HashMap<>();
            main.setValue(main.fieldByName("go"), vm.mirrorOf(1));

            boolean ended = false;
            long until = System.currentTimeMillis() + 240_000;
            while (!ended && System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(2_000);
                if (set == null) {
                    continue;
                }
                for (Event e : set) {
                    if (e instanceof BreakpointEvent b) {
                        String target = names.get(e.request());
                        int hit = hits.merge(target, 1, Integer::sum);
                        boolean filtered = hit == 1;
                        String row = target + (filtered ? " out filtered" : " out bare");
                        StepRequest step = erm.createStepRequest(b.thread(),
                                StepRequest.STEP_LINE, StepRequest.STEP_OUT);
                        if (filtered) {
                            step.addClassExclusionFilter("java.*");
                            step.addClassExclusionFilter("jdk.*");
                            step.addClassExclusionFilter("sun.*");
                        }
                        step.addCountFilter(1);
                        step.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
                        names.put(step, row);
                        step.enable();
                    } else if (e instanceof StepEvent s) {
                        System.out.println("  " + names.get(e.request()) + ": " + at(s.location()));
                        erm.deleteEventRequest(e.request());
                    } else if (e instanceof VMDeathEvent || e instanceof VMDisconnectEvent) {
                        ended = true;
                    }
                }
                if (!ended) {
                    set.resume();
                }
            }
            System.out.println("== end");
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
                    System.out.println("FAILED: class " + name + " never loaded");
                    System.exit(2);
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
    }
}
