// Interpreter round i5 wave 105, lane L1: JDWP `ReferenceType.Fields` /
// `Methods` / `Interfaces` of a JDK class loaded by
// `Class.forName(name, false, null)` and not yet linked.
//
// HotSpot's back end answers them from JVMTI `GetClassFields`,
// `GetClassMethods` and `GetImplementedInterfaces`, which refuse a class
// whose status has neither PREPARED nor ARRAY: `CLASS_NOT_PREPARED` (22),
// `ClassNotPreparedException` in JDI. L1W103JdiUnpreparedClassMembers proves
// it for an application class; a bootstrap JDK class links trivially here
// (verification skipped), and since wave 104 one defined while JDI's
// `ClassPrepare` request is in force is refused too
// (vm/src/debug/mod.rs `jdwp_class_unprepared`, the watermark;
// vm/src/debug/commands.rs `read_prepared_class`). No probe read its members
// (the i103 page's verification note). Page:
// docs/internal/fixed-bugs/interpreter-i103-L1-jdwp-members-of-an-unprepared-class-are-answered-FIXED-20261016.md
//
// `java.time.chrono.MinguoEra` is loaded by nothing at start-up (nor by
// JDI's attach), so the probe's `forName` defines it. Its field and method
// counts are shown as `some` (the JDK's own members may change); it
// implements one interface, `java.time.chrono.Era`.
//
// Canonical transcript: no ids, no addresses, no timings.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   ended
//
// Under a debugger (tools/jdi/run-jdi-conformance.sh runs it):
//
//   javac -g -d out L1W105JdiUnlinkedJdkClassMembers.java
//   cratonvm --java-home $JDK --jdwp-port 5803 -cp out L1W105JdiUnlinkedJdkClassMembers wait
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5803 //       -cp out L1W105JdiUnlinkedJdkClassMembers wait
//   java -cp out L1W105JdiUnlinkedJdkClassMembers debug 5803 > transcript.txt
//
// HotSpot 25 as the debuggee (derived from ReferenceTypeImpl.c and
// jvmtiEnv.cpp, NOT measured):
//
//   == attach
//   == loaded, not linked
//     name=java.time.chrono.MinguoEra prepared=false
//     fields: ClassNotPreparedException
//     methods: ClassNotPreparedException
//     interfaces: ClassNotPreparedException
//   == initialized
//     prepared=true
//     fields: some
//     methods: some
//     interfaces: 1
//   == end
//     disconnected
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.util.*;

public class L1W105JdiUnlinkedJdkClassMembers {
    /** Set by the debugger: 1 = start, 3 = initialize, 5 = done. 2 and 4 are the debuggee's acks. */
    static volatile int phase;
    static Class<?> lazy;

    static final String LAZY = "java.time.chrono.MinguoEra";

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean debugged = args.length == 1 && args[0].equals("wait");
        long until = System.currentTimeMillis() + 300_000;
        while (debugged && phase == 0 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        lazy = Class.forName(LAZY, false, null);
        if (debugged) {
            phase = 2;
        }
        while (debugged && phase != 3 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        Class.forName(LAZY, true, null);
        if (debugged) {
            phase = 4;
        }
        while (debugged && phase != 5 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        System.out.println("ended");
    }

    static final class Debugger {
        static VirtualMachine vm;
        static ClassType type;

        static void await(int value) throws Exception {
            long until = System.currentTimeMillis() + 120_000;
            while (((IntegerValue) type.getValue(type.fieldByName("phase"))).value() != value) {
                if (System.currentTimeMillis() > until) {
                    fail("phase " + value + " never came");
                }
                Thread.sleep(20);
            }
        }

        static String members(java.util.concurrent.Callable<List<?>> list) {
            try {
                return Integer.toString(list.call().size());
            } catch (Exception e) {
                return e.getClass().getSimpleName();
            }
        }

        /** A count is shown as whether it is non-zero (the JDK's own members may change). */
        static String some(String answer) {
            try {
                return Integer.parseInt(answer) > 0 ? "some" : "none";
            } catch (NumberFormatException exception) {
                return answer;
            }
        }

        static void rows(ClassType lazyType) {
            System.out.println("  fields: " + some(members(() -> new ArrayList<>(lazyType.fields()))));
            System.out.println("  methods: " + some(members(() -> new ArrayList<>(lazyType.methods()))));
            System.out.println("  interfaces: " + members(() -> new ArrayList<>(lazyType.interfaces())));
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
            System.out.println("== attach");
            type = (ClassType) awaitClass("L1W105JdiUnlinkedJdkClassMembers");
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(1));
            await(2);
            System.out.println("== loaded, not linked");
            ClassObjectReference mirror = (ClassObjectReference) type.getValue(type.fieldByName("lazy"));
            ClassType lazyType = (ClassType) mirror.reflectedType();
            System.out.println("  name=" + lazyType.name() + " prepared=" + lazyType.isPrepared());
            rows(lazyType);
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(3));
            await(4);
            System.out.println("== initialized");
            System.out.println("  prepared=" + lazyType.isPrepared());
            rows(lazyType);
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(5));
            System.out.println("== end");
            boolean ended = false;
            while (!ended) {
                EventSet next;
                try {
                    next = vm.eventQueue().remove(60_000);
                } catch (VMDisconnectedException gone) {
                    break;
                }
                if (next == null) {
                    fail("the debuggee did not end");
                }
                for (Event e : next) {
                    if (e instanceof VMDisconnectEvent) {
                        ended = true;
                    }
                }
                if (!ended) {
                    next.resume();
                }
            }
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
