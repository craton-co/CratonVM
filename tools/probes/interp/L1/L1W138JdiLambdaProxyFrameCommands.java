// Interpreter round i7 wave 138, lane L1: JDI frame commands on a thread
// stopped in a lambda's target, called through its functional interface
// (`IntSupplier.getAsInt`), where the listing has the spun proxy's frame
// between the target and its caller `work`.
//
// Rows (one per call of the target, at a breakpoint on its first line):
//
//   frames:           the thread's top three frames.
//   set below proxy:  `StackFrame.setValue` of `work`'s `bonus` (frame 2) to
//                     100; the debuggee's final sum shows it (`sum=109`).
//   pop target:       `ThreadReference.popFrames(frame 0)`; HotSpot pops the
//                     target and re-runs the proxy's invoke, so the
//                     breakpoint is hit again (`hits`).
//
// CratonVM before wave 138 (predicted from the code, not run): the proxy's
// row was counted as an interpreter frame by `SetValues`, which then found
// the frame under `work` and refused (`INVALID_FRAMEID`, JDI
// `InvalidStackFrameException`). Since wave 138 the write lands
// (`CRATONVM_JDWP_LAMBDA_PROXY_STEPS`); the pop of the target was refused
// (`OPAQUE_FRAME`) until wave 139, which serves it
// (`CRATONVM_JDWP_LAMBDA_TARGET_POP`, default ON; `=0` gives `pop target:
// com.sun.jdi.NativeMethodException` and `hits=3`),
// docs/feature-designs/jvmti-delivery-threading.md
//
// Canonical transcript: no ids, no addresses, no timings. A hidden class's
// name is cut at its `/`.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   sum=9
//
// Under a debugger (the debuggee waits up to five minutes):
//
//   javac -g -d out L1W138JdiLambdaProxyFrameCommands.java
//   cratonvm --java-home $JDK --jdwp-port 5799 -cp out L1W138JdiLambdaProxyFrameCommands wait
//   #   (HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5799 -cp out L1W138JdiLambdaProxyFrameCommands wait)
//   java -cp out L1W138JdiLambdaProxyFrameCommands debug 5799 > transcript.txt     # HotSpot 25's java
//   # both modes (--compatible too), default and --nojit; the debuggee's
//   # stdout (`sum=...`) is part of the comparison
//
// HotSpot 25.0.3 as the debuggee (MEASURED on the Windows box, default flags
// and -Xint, identical); the debuggee printed `sum=109`. CratonVM expected
// after wave 139 (not run): the same block.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, Windows, default flags)
// == attach
//   frames: Main.lambdaTarget $Lambda.getAsInt Main.work
//   set below proxy: ok, reads 100
//   pop target: ok, top $Lambda.getAsInt
//   hits=4
// == end
// HOTSPOT_EXPECTED_END
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;
import java.util.function.IntSupplier;

public class L1W138JdiLambdaProxyFrameCommands {
    /** 0 until the debugger has set its breakpoints. */
    static volatile int go;

    static int lambdaTarget() {
        int y = 3;
        return y;
    }

    static int work() {
        IntSupplier s = L1W138JdiLambdaProxyFrameCommands::lambdaTarget;
        int bonus = 0;
        int sum = 0;
        for (int round = 0; round < 3; round++) {
            sum += s.getAsInt();
        }
        return sum + bonus;
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
        static final String MAIN = "L1W138JdiLambdaProxyFrameCommands";
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
            Location first = main.methodsByName("lambdaTarget").get(0).allLineLocations().get(0);
            BreakpointRequest bp = erm.createBreakpointRequest(first);
            bp.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            bp.enable();
            main.setValue(main.fieldByName("go"), vm.mirrorOf(1));

            int hits = 0;
            boolean ended = false;
            long until = System.currentTimeMillis() + 240_000;
            while (!ended && System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(2_000);
                if (set == null) {
                    continue;
                }
                for (Event e : set) {
                    if (e instanceof BreakpointEvent b) {
                        hits++;
                        ThreadReference t = b.thread();
                        if (hits == 1) {
                            StringBuilder sb = new StringBuilder("  frames:");
                            for (int i = 0; i < 3; i++) {
                                sb.append(' ').append(name(t.frame(i).location()));
                            }
                            System.out.println(sb);
                            try {
                                StackFrame w = t.frame(2);
                                LocalVariable bonus = w.visibleVariableByName("bonus");
                                w.setValue(bonus, vm.mirrorOf(100));
                                System.out.println("  set below proxy: ok, reads "
                                        + t.frame(2).getValue(t.frame(2).visibleVariableByName("bonus")));
                            } catch (Exception x) {
                                System.out.println("  set below proxy: " + x.getClass().getName());
                            }
                        } else if (hits == 2) {
                            try {
                                t.popFrames(t.frame(0));
                                System.out.println("  pop target: ok, top " + name(t.frame(0).location()));
                            } catch (Exception x) {
                                System.out.println("  pop target: " + x.getClass().getName());
                            }
                        }
                    } else if (e instanceof VMDeathEvent || e instanceof VMDisconnectEvent) {
                        ended = true;
                    }
                }
                if (!ended) {
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
