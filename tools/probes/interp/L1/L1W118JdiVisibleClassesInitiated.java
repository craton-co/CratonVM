// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 118, lane L1 -- JDWP
// `ClassLoaderReference.VisibleClasses` lists the classes the loader is an
// INITIATING loader of (HotSpot: JVMTI `GetClassLoaderClasses`), not every
// class it could reach by delegation; and JDI's type lookup through that list
// (`ClassLoaderReferenceImpl.findType`) makes `Field.type()` of a loaded JDK
// type the program never resolved throw `ClassNotLoadedException`, as on
// HotSpot. `CRATONVM_JDWP_VISIBLE_CLASSES_INITIATED` (default OFF until this
// probe and the JDI suite agree; the record is `Instrumentation.getInitiatedClasses`'
// `NativeContextImpl::list_initiated_class_ids`). Page:
// docs/internal/fixed-bugs/interpreter-i41-L1-proposal-one-initiating-loader-record-for-every-loader-DONE-20261224.md,
// "Wave 118".
//
// The debuggee declares a static field of type `ConcurrentHashMap` (loaded
// by the JDK at boot) that it never assigns and whose class it never names
// in a constant (only the field's descriptor names it), and one of type
// `java.util.ArrayList`, which it does construct.
//
// Plain run, both modes: `ended`.
// Under a debugger (debug build, both modes, the switch unset and =1):
//   javac -g -d out L1W118JdiVisibleClassesInitiated.java
//   cratonvm [--compatible] --java-home $JDK --jdwp-port 5794 -cp out L1W118JdiVisibleClassesInitiated wait &
//   java -cp out L1W118JdiVisibleClassesInitiated debug 5794
//   (HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5794 -cp out L1W118JdiVisibleClassesInitiated wait)
//
// HotSpot 25.0.3 as the debuggee (run in wave 118, Windows):
//   == attach
//     visible L1W118JdiVisibleClassesInitiated=true
//     visible java.lang.Object=true
//     visible java.lang.String=true
//     visible java.util.ArrayList=true
//     visible int[]=true
//     visible java.util.concurrent.ConcurrentHashMap=false
//     visible java.util.zip.CRC32=false
//     resolved.type()=java.util.ArrayList
//     neverResolved.type() threw ClassNotLoadedException java.util.concurrent.ConcurrentHashMap
//   == end
//     disconnected
//
// CratonVM with the switch off (every class the loader reaches by
// delegation, the answer since wave 24): the two `=false` rows read true and
// `neverResolved.type()` answers the type.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.util.*;

public class L1W118JdiVisibleClassesInitiated {
    static volatile int go;
    static java.util.concurrent.ConcurrentHashMap<String, String> neverResolved;
    static ArrayList<String> resolved = new ArrayList<>();

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        resolved.add("x");
        if (args.length == 1 && args[0].equals("wait")) {
            long until = System.currentTimeMillis() + 300_000;
            while (go == 0 && System.currentTimeMillis() < until) {
                Thread.sleep(10);
            }
        }
        System.out.println("ended");
    }

    static final class Debugger {
        static VirtualMachine vm;

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
            ReferenceType type = awaitClass("L1W118JdiVisibleClassesInitiated");
            ClassLoaderReference loader = type.classLoader();
            Set<String> visible = new HashSet<>();
            for (ReferenceType r : loader.visibleClasses()) {
                visible.add(r.name());
            }
            for (String n : List.of("L1W118JdiVisibleClassesInitiated", "java.lang.Object",
                    "java.lang.String", "java.util.ArrayList", "int[]",
                    "java.util.concurrent.ConcurrentHashMap", "java.util.zip.CRC32")) {
                System.out.println("  visible " + n + "=" + visible.contains(n));
            }
            for (String f : List.of("resolved", "neverResolved")) {
                try {
                    System.out.println("  " + f + ".type()=" + type.fieldByName(f).type().name());
                } catch (ClassNotLoadedException e) {
                    System.out.println("  " + f + ".type() threw ClassNotLoadedException "
                            + e.className());
                }
            }
            ((ClassType) type).setValue(type.fieldByName("go"), vm.mirrorOf(1));
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
                if (!found.isEmpty() && found.get(0).isInitialized()) {
                    return found.get(0);
                }
                if (System.currentTimeMillis() > until) {
                    fail("class " + name + " never initialized");
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
