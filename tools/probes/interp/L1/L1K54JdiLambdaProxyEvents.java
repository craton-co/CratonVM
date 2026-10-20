// The i54-L1 retirement: a JDWP session's events for the classes the VM
// mints for lambdas, against HotSpot
// (docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-does-not-know-the-vms-lambda-proxy-classes-FIXED-20261007.md).
//
// The debugger enables a `ClassPrepare`, a `MethodEntry` and a `MethodExit`
// request filtered to `L1K54JdiLambdaProxyEvents*` (which a lambda's class
// matches: its name is the host's plus `$$Lambda/0x...`), lets the debuggee
// create and call a method reference and a capturing lambda, and prints the
// events in order (the declaring type's name masked, each method event's
// code index).
//
//   javac -g -d out L1K54JdiLambdaProxyEvents.java
//   cratonvm --jdwp-port 5885 -cp out L1K54JdiLambdaProxyEvents wait
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5885 -cp out L1K54JdiLambdaProxyEvents wait
//   java -cp out L1K54JdiLambdaProxyEvents debug 5885
//
// Plain run (no debugger): stdout must equal HotSpot 25's: `ended`.
// Expected debugger transcript (HotSpot 25.0.3 as the debuggee): at the end
// of the file.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;
import java.util.function.*;

public class L1K54JdiLambdaProxyEvents {
    static volatile int go;
    static int counter;
    static void hit() { counter++; }
    public static void main(String[] args) throws Exception {
        if (args.length == 2) { debug(Integer.parseInt(args[1])); return; }
        long deadline = System.currentTimeMillis() + 60000;
        while (go == 0 && System.currentTimeMillis() < deadline) Thread.sleep(5);
        Runnable r = L1K54JdiLambdaProxyEvents::hit;
        r.run();
        String s = "x";
        Supplier<String> sup = () -> s + counter;
        sup.get();
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
        ClassType type = (ClassType) vm.classesByName("L1K54JdiLambdaProxyEvents").get(0);
        EventRequestManager erm = vm.eventRequestManager();
        ClassPrepareRequest cp = erm.createClassPrepareRequest();
        cp.addClassFilter("L1K54JdiLambdaProxyEvents*");
        cp.setSuspendPolicy(EventRequest.SUSPEND_NONE);
        cp.enable();
        MethodEntryRequest me = erm.createMethodEntryRequest();
        me.addClassFilter("L1K54JdiLambdaProxyEvents*");
        me.setSuspendPolicy(EventRequest.SUSPEND_NONE);
        me.enable();
        MethodExitRequest mx = erm.createMethodExitRequest();
        mx.addClassFilter("L1K54JdiLambdaProxyEvents*");
        mx.setSuspendPolicy(EventRequest.SUSPEND_NONE);
        mx.enable();
        type.setValue(type.fieldByName("go"), vm.mirrorOf(1));
        List<String> lines = new ArrayList<>();
        boolean done = false;
        while (!done) {
            EventSet set = vm.eventQueue().remove(20000);
            if (set == null) break;
            for (Event e : set) {
                if (e instanceof ClassPrepareEvent) lines.add("prepare " + mask(((ClassPrepareEvent) e).referenceType().name()));
                else if (e instanceof MethodEntryEvent) { Method mm = ((MethodEntryEvent) e).method(); lines.add("entry " + mask(mm.declaringType().name()) + "." + mm.name() + " at " + ((MethodEntryEvent) e).location().codeIndex()); }
                else if (e instanceof MethodExitEvent) { Method mm = ((MethodExitEvent) e).method(); lines.add("exit " + mask(mm.declaringType().name()) + "." + mm.name() + " at " + ((MethodExitEvent) e).location().codeIndex()); if (mm.name().equals("main")) done = true; }
                else if (e instanceof VMDeathEvent || e instanceof VMDisconnectEvent) done = true;
            }
            set.resume();
        }
        for (String l : lines) System.out.println(l);
        try { vm.dispose(); } catch (Exception e) {}
    }
}

// Expected debugger transcript (HotSpot 25.0.3 as the debuggee, default flags):
//
//   prepare L1K54JdiLambdaProxyEvents$$Lambda/0x?
//   entry L1K54JdiLambdaProxyEvents$$Lambda/0x?.<init> at 0
//   exit L1K54JdiLambdaProxyEvents$$Lambda/0x?.<init> at 4
//   entry L1K54JdiLambdaProxyEvents$$Lambda/0x?.run at 0
//   entry L1K54JdiLambdaProxyEvents.hit at 0
//   exit L1K54JdiLambdaProxyEvents.hit at 8
//   exit L1K54JdiLambdaProxyEvents$$Lambda/0x?.run at 3
//   prepare L1K54JdiLambdaProxyEvents$$Lambda/0x?
//   entry L1K54JdiLambdaProxyEvents$$Lambda/0x?.<init> at 0
//   exit L1K54JdiLambdaProxyEvents$$Lambda/0x?.<init> at 9
//   entry L1K54JdiLambdaProxyEvents$$Lambda/0x?.get at 0
//   entry L1K54JdiLambdaProxyEvents.lambda$main$0 at 0
//   exit L1K54JdiLambdaProxyEvents.lambda$main$0 at 9
//   exit L1K54JdiLambdaProxyEvents$$Lambda/0x?.get at 7
//   exit L1K54JdiLambdaProxyEvents.main at 88
