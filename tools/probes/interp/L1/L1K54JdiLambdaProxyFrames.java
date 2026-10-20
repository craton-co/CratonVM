// The i54-L1 retirement: a lambda's class and its method's frame under a
// debugger (JDWP), against HotSpot
// (docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-does-not-know-the-vms-lambda-proxy-classes-FIXED-20261007.md,
// "Next step (from wave 60)" item 1).
//
// The debuggee calls a `Runnable` method reference, a `Function` method
// reference and a capturing `Supplier` lambda, whose targets (`hit`,
// `boxedHit`, `capturedHit`) hold a breakpoint. At each
// stop the debugger lists the thread's frames down to `main` (declaring type,
// method, code index), and for the lambda class's frame: `thisObject()`
// (the same object as the lambda?), its argument values, its visible
// variables, and its declaring type's facts (signature, modifiers, methods,
// fields, interfaces, loader, source name, whether `allClasses` lists it).
// The class names' hidden-class suffix is masked.
//
//   javac -g -d out L1K54JdiLambdaProxyFrames.java
//   cratonvm --jdwp-port 5874 -cp out L1K54JdiLambdaProxyFrames wait
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5874 -cp out L1K54JdiLambdaProxyFrames wait
//   java -cp out L1K54JdiLambdaProxyFrames debug 5874
//
// Plain run (no debugger): stdout must equal HotSpot 25's: `ended`.
// Expected debugger transcript (HotSpot 25.0.3 as the debuggee): at the end
// of the file.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;
import java.util.function.Function;
import java.util.function.Supplier;

public class L1K54JdiLambdaProxyFrames {
    static volatile int go;
    static Runnable plain;
    static Function<Integer, String> boxed;
    static Integer argument;
    static Supplier<String> capturing;
    static int counter;

    static void hit() {
        counter++;
    }

    static String boxedHit(Integer i) {
        counter += i;
        return "v";
    }

    static String capturedHit(String s, long n) {
        counter++;
        return s + n;
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean debugged = args.length == 1 && args[0].equals("wait");
        plain = L1K54JdiLambdaProxyFrames::hit;
        boxed = L1K54JdiLambdaProxyFrames::boxedHit;
        argument = 1234;
        String text = "cap";
        long number = 42L;
        capturing = () -> capturedHit(text, number);
        long deadline = System.currentTimeMillis() + 300_000;
        while (debugged && go == 0 && System.currentTimeMillis() < deadline) {
            Thread.sleep(5);
        }
        plain.run();
        boxed.apply(argument);
        capturing.get();
        System.out.println("ended");
    }

    static final class Debugger {
        static String mask(String s) {
            return s.replaceAll("0x[0-9a-f]+", "0x?");
        }

        static void run(int port) throws Exception {
            AttachingConnector connector = null;
            for (AttachingConnector c : Bootstrap.virtualMachineManager().attachingConnectors()) {
                if (c.transport().name().equals("dt_socket")) {
                    connector = c;
                }
            }
            Map<String, Connector.Argument> arguments = connector.defaultArguments();
            arguments.get("port").setValue(String.valueOf(port));
            arguments.get("hostname").setValue("localhost");
            VirtualMachine vm = null;
            for (int i = 0; i < 200 && vm == null; i++) {
                try {
                    vm = connector.attach(arguments);
                } catch (java.io.IOException e) {
                    Thread.sleep(100);
                }
            }
            System.out.println("== attach");
            ClassType type = (ClassType) vm.classesByName("L1K54JdiLambdaProxyFrames").get(0);
            EventRequestManager erm = vm.eventRequestManager();
            for (String name : new String[] {"hit", "boxedHit", "capturedHit"}) {
                Method m = type.methodsByName(name).get(0);
                BreakpointRequest bp = erm.createBreakpointRequest(m.location());
                bp.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
                bp.enable();
            }
            type.setValue(type.fieldByName("go"), vm.mirrorOf(1));
            int stops = 0;
            while (stops < 3) {
                EventSet set = vm.eventQueue().remove(60_000);
                if (set == null) {
                    System.out.println("== timeout");
                    break;
                }
                for (Event event : set) {
                    if (event instanceof BreakpointEvent) {
                        stops++;
                        report((BreakpointEvent) event, type, vm);
                    }
                }
                set.resume();
            }
            System.out.println("== end");
            vm.dispose();
            System.out.println("  disconnected");
        }

        static void report(BreakpointEvent event, ClassType type, VirtualMachine vm) throws Exception {
            ThreadReference thread = event.thread();
            String method = event.location().method().name();
            System.out.println("== stop in " + method);
            List<StackFrame> frames = thread.frames();
            ObjectReference lambda = (ObjectReference) type.getValue(type.fieldByName(method.equals("hit") ? "plain" : method.equals("boxedHit") ? "boxed" : "capturing"));
            Value arg = type.getValue(type.fieldByName("argument"));
            for (int i = 0; i < frames.size(); i++) {
                StackFrame frame = frames.get(i);
                Location at = frame.location();
                Method m = at.method();
                System.out.println("  frame " + i + ": " + mask(at.declaringType().name()) + "." + m.name() + m.signature()
                        + " at " + at.codeIndex());
                if (m.name().equals("main")) {
                    break;
                }
                if (!at.declaringType().name().contains("$$Lambda")) {
                    continue;
                }
                ObjectReference self = frame.thisObject();
                System.out.println("    this: " + (self == null ? "null" : self.equals(lambda) ? "the lambda" : "other"));
                try {
                    List<Value> values = frame.getArgumentValues();
                    StringBuilder s = new StringBuilder();
                    for (Value v : values) {
                        s.append(v == null ? "null" : v.equals(arg) ? "the argument" : v.toString()).append(' ');
                    }
                    System.out.println("    arguments: " + values.size() + " " + s.toString().trim());
                } catch (Exception e) {
                    System.out.println("    arguments: " + e.getClass().getSimpleName());
                }
                try {
                    System.out.println("    variables: " + frame.visibleVariables().size());
                } catch (AbsentInformationException e) {
                    System.out.println("    variables: absent");
                }
                ReferenceType rt = at.declaringType();
                System.out.println("    signature: " + mask(rt.signature()));
                System.out.println("    modifiers: 0x" + Integer.toHexString(rt.modifiers()));
                StringBuilder ms = new StringBuilder();
                for (Method x : rt.methods()) {
                    ms.append(x.name()).append(x.signature()).append(' ');
                }
                System.out.println("    methods: " + ms.toString().trim());
                StringBuilder fs = new StringBuilder();
                for (Field f : rt.fields()) {
                    fs.append(f.name()).append(':').append(f.signature()).append(' ');
                }
                System.out.println("    fields: " + fs.toString().trim());
                StringBuilder vs = new StringBuilder();
                for (Field f : rt.fields()) {
                    vs.append(self == null ? "?" : String.valueOf(self.getValue(f))).append(' ');
                }
                System.out.println("    field values: " + vs.toString().trim());
                StringBuilder is = new StringBuilder();
                for (InterfaceType t : ((ClassType) rt).interfaces()) {
                    is.append(t.name()).append(' ');
                }
                System.out.println("    interfaces: " + is.toString().trim());
                System.out.println("    loader: " + (rt.classLoader() != null && rt.classLoader().equals(type.classLoader()) ? "the host's" : String.valueOf(rt.classLoader())));
                try {
                    System.out.println("    source: " + rt.sourceName());
                } catch (AbsentInformationException e) {
                    System.out.println("    source: absent");
                }
                System.out.println("    object's type: " + (lambda.referenceType().equals(rt) ? "this type" : mask(lambda.referenceType().name())));
                System.out.println("    in allClasses: " + vm.allClasses().contains(rt));
                System.out.println("    superclass: " + ((ClassType) rt).superclass().name());
                System.out.println("    bytecodes: " + m.bytecodes().length);
                System.out.println("    status: prepared=" + rt.isPrepared() + " initialized=" + rt.isInitialized() + " verified=" + rt.isVerified());
                System.out.println("    line: " + at.lineNumber());
            }
        }
    }
}

// Expected debugger transcript (HotSpot 25.0.3 as the debuggee, default flags):
//
//   == attach
//   == stop in hit
//     frame 0: L1K54JdiLambdaProxyFrames.hit()V at 0
//     frame 1: L1K54JdiLambdaProxyFrames$$Lambda/0x?.run()V at 0
//       this: the lambda
//       arguments: 0 
//       variables: absent
//       signature: LL1K54JdiLambdaProxyFrames$$Lambda.0x?;
//       modifiers: 0x1030
//       methods: <init>()V run()V
//       fields: 
//       field values: 
//       interfaces: java.lang.Runnable
//       loader: the host's
//       source: absent
//       object's type: this type
//       in allClasses: true
//       superclass: java.lang.Object
//       bytecodes: 4
//       status: prepared=true initialized=true verified=true
//       line: -1
//     frame 2: L1K54JdiLambdaProxyFrames.main([Ljava/lang/String;)V at 132
//   == stop in boxedHit
//     frame 0: L1K54JdiLambdaProxyFrames.boxedHit(Ljava/lang/Integer;)Ljava/lang/String; at 0
//     frame 1: L1K54JdiLambdaProxyFrames$$Lambda/0x?.apply(Ljava/lang/Object;)Ljava/lang/Object; at 4
//       this: the lambda
//       arguments: 1 the argument
//       variables: absent
//       signature: LL1K54JdiLambdaProxyFrames$$Lambda.0x?;
//       modifiers: 0x1030
//       methods: <init>()V apply(Ljava/lang/Object;)Ljava/lang/Object;
//       fields: 
//       field values: 
//       interfaces: java.util.function.Function
//       loader: the host's
//       source: absent
//       object's type: this type
//       in allClasses: true
//       superclass: java.lang.Object
//       bytecodes: 8
//       status: prepared=true initialized=true verified=true
//       line: -1
//     frame 2: L1K54JdiLambdaProxyFrames.main([Ljava/lang/String;)V at 143
//   == stop in capturedHit
//     frame 0: L1K54JdiLambdaProxyFrames.capturedHit(Ljava/lang/String;J)Ljava/lang/String; at 0
//     frame 1: L1K54JdiLambdaProxyFrames.lambda$main$0(Ljava/lang/String;J)Ljava/lang/String; at 2
//     frame 2: L1K54JdiLambdaProxyFrames$$Lambda/0x?.get()Ljava/lang/Object; at 8
//       this: the lambda
//       arguments: 0 
//       variables: absent
//       signature: LL1K54JdiLambdaProxyFrames$$Lambda.0x?;
//       modifiers: 0x1030
//       methods: <init>(Ljava/lang/String;J)V get()Ljava/lang/Object;
//       fields: arg$1:Ljava/lang/String; arg$2:J
//       field values: "cap" 42
//       interfaces: java.util.function.Supplier
//       loader: the host's
//       source: absent
//       object's type: this type
//       in allClasses: true
//       superclass: java.lang.Object
//       bytecodes: 12
//       status: prepared=true initialized=true verified=true
//       line: -1
//     frame 3: L1K54JdiLambdaProxyFrames.main([Ljava/lang/String;)V at 152
//   == end
//     disconnected
