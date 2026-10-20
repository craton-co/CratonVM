// The i54-L1 retirement: a debugger's listing of a thread started with a
// lambda, against HotSpot
// (docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-does-not-know-the-vms-lambda-proxy-classes-FIXED-20261007.md).
//
// The thread runs `() -> body()`; a breakpoint in `body` stops it and the
// debugger lists its frames: the body, the lambda, the lambda class's `run`,
// then `Thread.runWith` (hidden from stack traces, listed by JDWP) and
// `Thread.run`. `--compatible` serves `Thread.run` without running its
// bytecode; its listing lists the frames the served call stands for.
//
//   javac -g -d out L1K54JdiLambdaThreadFrames.java
//   cratonvm --jdwp-port 5893 -cp out L1K54JdiLambdaThreadFrames wait
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5893 -cp out L1K54JdiLambdaThreadFrames wait
//   java -cp out L1K54JdiLambdaThreadFrames debug 5893
//
// Plain run (no debugger): stdout must equal HotSpot 25's: `ended`.
// Expected debugger transcript (HotSpot 25.0.3 as the debuggee): at the end
// of the file.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1K54JdiLambdaThreadFrames {
    static volatile int go;
    static int counter;
    static void body() { counter++; }
    public static void main(String[] args) throws Exception {
        if (args.length == 2) { debug(Integer.parseInt(args[1])); return; }
        long deadline = System.currentTimeMillis() + 60000;
        while (go == 0 && System.currentTimeMillis() < deadline) Thread.sleep(5);
        Thread t = new Thread(() -> body(), "lambda-thread");
        t.start();
        t.join();
        System.out.println("ended");
    }
    static String mask(String s) { return s.replaceAll("0x[0-9a-f]+", "0x?"); }
    static void debug(int port) throws Exception {
        AttachingConnector c = null;
        for (AttachingConnector a : Bootstrap.virtualMachineManager().attachingConnectors()) if (a.transport().name().equals("dt_socket")) c = a;
        Map<String, Connector.Argument> m = c.defaultArguments();
        m.get("port").setValue(String.valueOf(port)); m.get("hostname").setValue("localhost");
        VirtualMachine vm = null;
        for (int i = 0; i < 200 && vm == null; i++) { try { vm = c.attach(m); } catch (java.io.IOException e) { Thread.sleep(100); } }
        ClassType type = (ClassType) vm.classesByName("L1K54JdiLambdaThreadFrames").get(0);
        BreakpointRequest bp = vm.eventRequestManager().createBreakpointRequest(type.methodsByName("body").get(0).location());
        bp.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
        bp.enable();
        type.setValue(type.fieldByName("go"), vm.mirrorOf(1));
        while (true) {
            EventSet set = vm.eventQueue().remove(30000);
            if (set == null) break;
            boolean hit = false;
            for (Event e : set) if (e instanceof BreakpointEvent) {
                hit = true;
                for (StackFrame f : ((BreakpointEvent) e).thread().frames()) {
                    Location l = f.location();
                    System.out.println("  " + mask(l.declaringType().name()) + "." + l.method().name() + " at " + l.codeIndex());
                }
            }
            set.resume();
            if (hit) break;
        }
        vm.dispose();
    }
}

// Expected debugger transcript (HotSpot 25.0.3 as the debuggee, default flags):
//
//     L1K54JdiLambdaThreadFrames.body at 0
//     L1K54JdiLambdaThreadFrames.lambda$main$0 at 0
//     L1K54JdiLambdaThreadFrames$$Lambda/0x?.run at 0
//     java.lang.Thread.runWith at 5
//     java.lang.Thread.run at 19
