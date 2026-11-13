// Interpreter round i7 wave 139, lane L1: JDI `popFrames` of a method whose
// caller is a hot loop, JIT-compiled before the debugger attaches
// (`CRATONVM_JDWP_COMPILED_CALLER_POP`).
//
// The debuggee warms `Main.hot` (a loop calling `Target.f`, a separate class,
// so a breakpoint in it does not withdraw `hot`), then waits for the
// debugger. The debugger sets a breakpoint in `Target.f`'s rare branch
// (taken once per `hot` call, at i == 777) and lets the debuggee call `hot`
// twice. At the first hit it pops `f`'s frame; HotSpot deoptimizes `hot` and
// re-runs its invoke, so the breakpoint is hit again with the same argument.
//
// CratonVM before wave 139 (predicted from the code, not run): on a JIT run
// whose call reaches the JIT helpers' interpreted-callee door, the pop is
// refused (`OPAQUE_FRAME`, JDI `NativeMethodException`) and `hits=2`. Under
// `--nojit` the caller is interpreted and the ordinary pop serves it.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   sum=50000
//
// Under a debugger (the debuggee warms, then waits up to five minutes):
//
//   javac -g -d out L1W139JdiPopFrameUnderCompiledCaller.java
//   cratonvm --java-home $JDK --jdwp-port 5800 -cp out L1W139JdiPopFrameUnderCompiledCaller wait
//   #   (HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5800 -cp out L1W139JdiPopFrameUnderCompiledCaller wait)
//   java -cp out L1W139JdiPopFrameUnderCompiledCaller debug 5800
//
// HotSpot 25.0.3 (MEASURED on the Windows box, default flags and -Xint,
// identical; the debuggee printed `sum=50000`):
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, Windows, default flags)
// == attach
//   frames: Target.f Main.hot
//   pop f: ok, top Main.hot
//   hits=3
// == end
// HOTSPOT_EXPECTED_END
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W139JdiPopFrameUnderCompiledCaller {
    static volatile int go;

    static final class Target {
        static int f(int i) {
            if (i == 777) {
                int z = i * 2;
                return z - i - 776;
            }
            return i & 1;
        }
    }

    static int hot(int n) {
        int sum = 0;
        for (int i = 0; i < n; i++) {
            sum += Target.f(i);
        }
        return sum;
    }

    public static void main(String[] args) throws Throwable {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        int warm = 0;
        for (int k = 0; k < 40; k++) {
            warm += hot(50_000);
        }
        if (args.length == 1 && args[0].equals("wait")) {
            long until = System.currentTimeMillis() + 300_000;
            while (go == 0 && System.currentTimeMillis() < until) {
                Thread.sleep(10);
            }
        }
        int sum = hot(50_000) + hot(50_000);
        System.out.println("sum=" + sum + (warm == 40 * 25_000 ? "" : " warm=" + warm));
    }

    static final class Debugger {
        static final String MAIN = "L1W139JdiPopFrameUnderCompiledCaller";
        static VirtualMachine vm;

        static String name(Location l) {
            String type = l.declaringType().name().replace(MAIN + "$", "").replace(MAIN, "Main");
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
            ReferenceType target = awaitClass(MAIN + "$Target");
            // The rare branch's first line: `int z = i * 2;`.
            Method f = target.methodsByName("f").get(0);
            int first = Integer.MAX_VALUE;
            for (Location l : f.allLineLocations()) {
                first = Math.min(first, l.lineNumber());
            }
            Location branch = f.locationsOfLine(first + 1).get(0);
            BreakpointRequest bp = erm.createBreakpointRequest(branch);
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
                            System.out.println("  frames: " + name(t.frame(0).location()) + " "
                                    + name(t.frame(1).location()));
                            try {
                                t.popFrames(t.frame(0));
                                System.out.println("  pop f: ok, top " + name(t.frame(0).location()));
                            } catch (Exception x) {
                                System.out.println("  pop f: " + x.getClass().getName());
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
