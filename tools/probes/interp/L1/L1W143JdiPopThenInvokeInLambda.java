// Interpreter round i7 wave 143, lane L1 (review of wave 139's lambda-target
// PopFrames, `CRATONVM_JDWP_LAMBDA_TARGET_POP`): after `popFrames` of a
// lambda's target, while the thread is still suspended there, the debugger
// invokes a static method on the thread (`ClassType.invokeMethod`) that hits a
// breakpoint of its own. That nested stop is resumed, the invocation returns,
// then the original stop is resumed. HotSpot re-runs the proxy's invoke, so
// the target's breakpoint is hit again.
//
// CratonVM before wave 143 (from the code, not run): the nested stop's resume
// took the pop's restart record (`pop_frames::take_door_restart` ran at the
// first resumed park), dropped it with the argument pins under the
// invocation's, and the original stop resumed with the door's entry frame
// gone: the loop indexed a frame that no longer existed. Since wave 143 only
// the park the pop was made at takes the record.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   sum=9
//
// Under a debugger (the debuggee waits up to five minutes):
//
//   javac -g -d out L1W143JdiPopThenInvokeInLambda.java
//   cratonvm [--compatible] [--nojit] --java-home $JDK --jdwp-port 5806 -cp out L1W143JdiPopThenInvokeInLambda wait
//   #   (HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5806 -cp out L1W143JdiPopThenInvokeInLambda wait)
//   java -cp out L1W143JdiPopThenInvokeInLambda debug 5806
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, Windows, MEASURED 2026-10-21 by the orchestrator; default flags and -Xint identical)
// == attach
//   pop target: ok, top $Lambda.getAsInt
//   nested stop: Main.helper
//   invoke: 42
//   hits=4
// == end
// HOTSPOT_EXPECTED_END
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;
import java.util.function.IntSupplier;

public class L1W143JdiPopThenInvokeInLambda {
    static volatile int go;

    static int helper() {
        int h = 42;
        return h;
    }

    static int lambdaTarget() {
        int y = 3;
        return y;
    }

    static int work() {
        IntSupplier s = L1W143JdiPopThenInvokeInLambda::lambdaTarget;
        int sum = 0;
        for (int round = 0; round < 3; round++) {
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
        static final String MAIN = "L1W143JdiPopThenInvokeInLambda";
        static VirtualMachine vm;

        static String name(Location l) {
            String type = l.declaringType().name();
            int slash = type.indexOf('/');
            if (slash >= 0) {
                type = type.substring(0, slash);
            }
            type = type.replace(MAIN + "$", "").replace(MAIN, "Main");
            return type + "." + l.method().name();
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
            BreakpointRequest target = erm.createBreakpointRequest(
                    main.methodsByName("lambdaTarget").get(0).allLineLocations().get(0));
            target.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            target.enable();
            BreakpointRequest nested = erm.createBreakpointRequest(
                    main.methodsByName("helper").get(0).allLineLocations().get(0));
            nested.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            nested.enable();
            main.setValue(main.fieldByName("go"), vm.mirrorOf(1));

            int hits = 0;
            boolean ended = false;
            EventSet held = null;
            Thread[] invoker = new Thread[1];
            long until = System.currentTimeMillis() + 240_000;
            while (!ended && System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(500);
                if (set == null) {
                    // The invocation returned: resume the stop it ran from.
                    if (held != null && invoker[0] != null && !invoker[0].isAlive()) {
                        EventSet resume = held;
                        held = null;
                        invoker[0] = null;
                        resume.resume();
                    }
                    continue;
                }
                boolean keep = false;
                for (Event e : set) {
                    if (e instanceof BreakpointEvent b && e.request() == target) {
                        hits++;
                        ThreadReference t = b.thread();
                        if (hits == 1) {
                            try {
                                t.popFrames(t.frame(0));
                                System.out.println("  pop target: ok, top " + name(t.frame(0).location()));
                            } catch (Exception x) {
                                System.out.println("  pop target: " + x.getClass().getName());
                            }
                            Method helper = main.methodsByName("helper").get(0);
                            invoker[0] = new Thread(() -> {
                                try {
                                    Value v = main.invokeMethod(t, helper, List.of(), 0);
                                    System.out.println("  invoke: " + v);
                                } catch (Exception x) {
                                    System.out.println("  invoke: " + x.getClass().getName());
                                }
                            });
                            invoker[0].start();
                            held = set;
                            keep = true;
                        }
                    } else if (e instanceof BreakpointEvent b && e.request() == nested) {
                        System.out.println("  nested stop: " + name(b.location()));
                    } else if (e instanceof VMDeathEvent || e instanceof VMDisconnectEvent) {
                        ended = true;
                    }
                }
                if (!ended && !keep) {
                    set.resume();
                }
            }
            System.out.println("  hits=" + hits);
            System.out.println("== end");
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
