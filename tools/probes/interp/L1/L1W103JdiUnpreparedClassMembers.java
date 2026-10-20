// Interpreter round i5 wave 103, lane L1: JDWP `ReferenceType.Fields` /
// `Methods` / `Interfaces` of a class loaded but not prepared.
//
// HotSpot's back end answers them from JVMTI `GetClassFields`,
// `GetClassMethods` and `GetImplementedInterfaces`, which refuse a class
// whose status has neither PREPARED nor ARRAY: `CLASS_NOT_PREPARED` (22).
// A debugger reaches such a class through its `Class` object
// (`ClassObjectReference.reflectedType()` of `Class.forName(n, false, l)`),
// not through `allClasses()` (L1W102JdiUnpreparedClassesHidden). CratonVM's
// server answered every class's members whatever its state
// (vm/src/debug/commands.rs `handle_rt_fields`, `handle_rt_methods`,
// `handle_rt_interfaces`); since wave 103 they refuse an unprepared class
// (`read_prepared_class`). Not built or run.
//
// Canonical transcript: no ids, no addresses, no timings.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   ended
//
// Under a debugger (tools/jdi/run-jdi-conformance.sh runs it):
//
//   javac -g -d out L1W103JdiUnpreparedClassMembers.java
//   cratonvm --java-home $JDK --jdwp-port 5794 -cp out L1W103JdiUnpreparedClassMembers wait
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5794 \
//       -cp out L1W103JdiUnpreparedClassMembers wait
//   java -cp out L1W103JdiUnpreparedClassMembers debug 5794 > transcript.txt
//
// HotSpot 25 as the debuggee (derived from ReferenceTypeImpl.c, jvmtiEnv.cpp
// and JDWPException.toJDIException, NOT measured):
//
//   == attach
//   == loaded, not linked
//     name=L1W103JdiUnpreparedClassMembers$Lazy prepared=false
//     fields: ClassNotPreparedException
//     methods: ClassNotPreparedException
//     interfaces: ClassNotPreparedException
//   == initialized
//     prepared=true
//     fields: 1
//     methods: 3
//     interfaces: 1
//   == end
//     disconnected
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.util.*;

public class L1W103JdiUnpreparedClassMembers {
    /** Set by the debugger: 1 = start, 3 = initialize, 5 = done. 2 and 4 are the debuggee's acks. */
    static volatile int phase;
    static Class<?> lazy;

    static final String LAZY = "L1W103JdiUnpreparedClassMembers$Lazy";

    static final class Lazy implements Runnable {
        static int value = 42;

        public void run() {
            value++;
        }
    }

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
        lazy = Class.forName(LAZY, false, L1W103JdiUnpreparedClassMembers.class.getClassLoader());
        if (debugged) {
            phase = 2;
        }
        while (debugged && phase != 3 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        Class.forName(LAZY, true, lazy.getClassLoader());
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

        static void rows(ClassType lazyType) {
            System.out.println("  fields: " + members(() -> new ArrayList<>(lazyType.fields())));
            System.out.println("  methods: " + members(() -> new ArrayList<>(lazyType.methods())));
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
            type = (ClassType) awaitClass("L1W103JdiUnpreparedClassMembers");
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
