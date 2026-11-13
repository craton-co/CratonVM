// Interpreter round i7 wave 138, lane L1: JDI steps (STEP_LINE) made in a
// lambda's target, called through its functional interface
// (`IntSupplier.getAsInt`), chained: each step's completion makes the
// chain's next step from where it stopped.
//
// On HotSpot the spun proxy class's `getAsInt` is a Java frame (no line
// table) between `work` and the target. Rows:
//
//   over:            STEP_OVER at the target's first line, which calls
//                    `helper`: completes on the target's next line (not in
//                    `helper`).
//   out filtered:    STEP_OUT with the IDE filters (`java.*`, `jdk.*`,
//                    `sun.*`), then a bare STEP_OUT from where it stopped.
//   over chain:      STEP_OVER three times from the target's first line.
//   into chain:      STEP_OVER, then STEP_INTO twice.
//   over in helper:  a breakpoint in `helper` (reached from the target),
//                    STEP_OUT twice.
//
// CratonVM before wave 138 (predicted from the code, not run): the step's
// starting depth counted the proxy's listed row (the i54-L1 retirement) and
// the interpreter's suspend point did not, so `over` stopped in `helper` and
// a STEP_OUT completed on the target's next line; no step completed in the
// proxy's method. `CRATONVM_JDWP_LAMBDA_PROXY_STEPS=0` restores that.
// docs/feature-designs/jvmti-delivery-threading.md
//
// Canonical transcript: no ids, no addresses, no timings. A hidden class's
// name is cut at its `/`; lines are relative to the method's first line
// (`-1` when the method has none).
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   sum=30
//
// Under a debugger (the debuggee waits up to five minutes):
//
//   javac -g -d out L1W138JdiLambdaProxySteps.java
//   cratonvm --java-home $JDK --jdwp-port 5798 -cp out L1W138JdiLambdaProxySteps wait
//   #   (HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5798 -cp out L1W138JdiLambdaProxySteps wait)
//   java -cp out L1W138JdiLambdaProxySteps debug 5798 > transcript.txt     # HotSpot 25's java
//   # both modes (--compatible too), default and --nojit
//
// HotSpot 25.0.3 as the debuggee (MEASURED on the Windows box, default flags
// and -Xint, identical):
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, Windows, default flags)
// == attach
//   over 1 over: Main.lambdaTarget:+1
//   out filtered 1 out filtered: $Lambda.getAsInt:-1
//   out filtered 2 out: Main.work:+3
//   over chain 1 over: Main.lambdaTarget:+1
//   over chain 2 over: $Lambda.getAsInt:-1
//   over chain 3 over: Main.work:+3
//   into chain 1 over: Main.lambdaTarget:+1
//   into chain 2 into: $Lambda.getAsInt:-1
//   into chain 3 into: Main.work:+3
//   over in helper 1 out: Main.lambdaTarget:+0
//   over in helper 2 out: $Lambda.getAsInt:-1
// == end
//   disconnected
// HOTSPOT_EXPECTED_END
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;
import java.util.function.IntSupplier;

public class L1W138JdiLambdaProxySteps {
    /** 0 until the debugger has set its breakpoints. */
    static volatile int go;

    static int helper() {
        int z = 5;
        return z;
    }

    static int lambdaTarget() {
        int y = helper();
        return y + 1;
    }

    static int work() {
        IntSupplier s = L1W138JdiLambdaProxySteps::lambdaTarget;
        int sum = 0;
        for (int round = 0; round < 5; round++) {
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
        static final String MAIN = "L1W138JdiLambdaProxySteps";
        static VirtualMachine vm;
        static ClassType main;

        /** Per call of the target: the row name and its chain of steps. */
        static final String[] ROWS = {"over", "out filtered", "over chain", "into chain", "over in helper"};
        static final int[][] CHAINS = {
            {StepRequest.STEP_OVER},
            {-StepRequest.STEP_OUT, StepRequest.STEP_OUT},
            {StepRequest.STEP_OVER, StepRequest.STEP_OVER, StepRequest.STEP_OVER},
            {StepRequest.STEP_OVER, StepRequest.STEP_INTO, StepRequest.STEP_INTO},
            {StepRequest.STEP_OUT, StepRequest.STEP_OUT},
        };

        static int firstLine(Method m) {
            try {
                int first = Integer.MAX_VALUE;
                for (Location l : m.allLineLocations()) {
                    first = Math.min(first, l.lineNumber());
                }
                return first == Integer.MAX_VALUE ? -1 : first;
            } catch (AbsentInformationException none) {
                return -1;
            }
        }

        static String at(Location l) {
            String type = l.declaringType().name();
            int slash = type.indexOf('/');
            if (slash >= 0) {
                type = type.substring(0, slash);
            }
            type = type.replace(MAIN + "$", "").replace(MAIN, "Main");
            int line = l.lineNumber();
            int first = firstLine(l.method());
            String shown = line > 0 && first > 0 ? "+" + (line - first) : Integer.toString(line);
            return type + "." + l.method().name() + ":" + shown;
        }

        static StepRequest step(EventRequestManager erm, ThreadReference t, int code) {
            boolean filtered = code < 0;
            int depth = Math.abs(code);
            StepRequest step = erm.createStepRequest(t, StepRequest.STEP_LINE, depth);
            if (filtered) {
                step.addClassExclusionFilter("java.*");
                step.addClassExclusionFilter("jdk.*");
                step.addClassExclusionFilter("sun.*");
            }
            step.addCountFilter(1);
            step.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            step.enable();
            return step;
        }

        static String depthName(int code) {
            switch (Math.abs(code)) {
                case StepRequest.STEP_OVER: return "over";
                case StepRequest.STEP_INTO: return "into";
                default: return "out";
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
            main = (ClassType) awaitClass(MAIN);
            Location targetFirst = main.methodsByName("lambdaTarget").get(0).allLineLocations().get(0);
            Location helperFirst = main.methodsByName("helper").get(0).allLineLocations().get(0);
            BreakpointRequest target = erm.createBreakpointRequest(targetFirst);
            target.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            target.enable();
            BreakpointRequest helper = erm.createBreakpointRequest(helperFirst);
            helper.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            main.setValue(main.fieldByName("go"), vm.mirrorOf(1));

            int call = -1;
            int link = 0;
            StepRequest pending = null;
            boolean ended = false;
            long until = System.currentTimeMillis() + 240_000;
            while (!ended && System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(2_000);
                if (set == null) {
                    continue;
                }
                for (Event e : set) {
                    if (e instanceof BreakpointEvent b && e.request() == target) {
                        call++;
                        link = 0;
                        if (call >= ROWS.length) {
                            continue;
                        }
                        if (call == ROWS.length - 1) {
                            // The last row starts in `helper`.
                            target.disable();
                            helper.enable();
                            continue;
                        }
                        pending = step(erm, b.thread(), CHAINS[call][0]);
                    } else if (e instanceof BreakpointEvent b && e.request() == helper) {
                        helper.disable();
                        pending = step(erm, b.thread(), CHAINS[call][0]);
                    } else if (e instanceof StepEvent s) {
                        int code = CHAINS[call][link];
                        System.out.println("  " + ROWS[call] + " " + (link + 1) + " "
                                + depthName(code) + (code < 0 ? " filtered" : "") + ": "
                                + at(s.location()));
                        erm.deleteEventRequest(e.request());
                        pending = null;
                        link++;
                        if (link < CHAINS[call].length) {
                            pending = step(erm, s.thread(), CHAINS[call][link]);
                        }
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
