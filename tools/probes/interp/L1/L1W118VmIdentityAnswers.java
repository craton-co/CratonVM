// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 118, lane L1 -- one VM identity, as HotSpot 25
// gives it, whoever asks (stage 2 of
// docs/internal/proposals/interpreter/i61-L1-proposal-one-vm-property-list-20261025.md):
// the `java.vm.*` properties, `RuntimeMXBean`'s getters (HotSpot's
// `RuntimeImpl` reads those same properties) and, under a debugger, JDWP
// `VirtualMachine.Version` (the back end's `version()`, MEASURED: vmVersion =
// `java.version`, vmName = `java.vm.name`, the description
// "Java Debug Wire Protocol (Reference Implementation) version M.m\n
// JVM Debug Interface version M.m\nJVM version V (N, I)" from
// `java.version`, `java.vm.name` and `java.vm.info`). Values differ
// between VMs; the probe prints only whether they agree, and the
// description with the debuggee's own strings replaced by their keys.
//
// Plain run, both modes (stdout must equal HotSpot 25's):
//   java L1W118VmIdentityAnswers
//   cratonvm [--compatible] L1W118VmIdentityAnswers
//
// Under a debugger (debug build):
//   javac -g -d out L1W118VmIdentityAnswers.java
//   cratonvm [--compatible] --java-home $JDK --jdwp-port 5793 -cp out L1W118VmIdentityAnswers wait &
//   java -cp out L1W118VmIdentityAnswers debug 5793
//   (HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5793 -cp out L1W118VmIdentityAnswers wait)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3 HotSpot, Windows, run in wave 118)
// spec name=Java Virtual Machine Specification vendor=Oracle Corporation version=25
// runtime-bean name-same=true version-same=true vendor-same=true
// runtime-bean spec-name-same=true spec-vendor-same=true spec-version-same=true
// done
// HOTSPOT_EXPECTED_END
//
// The debugger's transcript on HotSpot 25.0.3 (run in wave 118):
//   == attach
//     vm.version() == java.version: true
//     vm.name() == java.vm.name: true
//     description: Java Debug Interface (Reference Implementation) version 25.0 |Java Debug Wire Protocol (Reference Implementation) version 25.0|JVM Debug Interface version 25.0|JVM version <java.version> (<java.vm.name>, <java.vm.info>)
//   == end
//     disconnected
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.lang.management.ManagementFactory;
import java.lang.management.RuntimeMXBean;
import java.util.*;

public class L1W118VmIdentityAnswers {
    static volatile int go;
    static String javaVersion = System.getProperty("java.version");
    static String vmName = System.getProperty("java.vm.name");
    static String vmInfo = System.getProperty("java.vm.info");

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        System.out.println("spec name=" + System.getProperty("java.vm.specification.name")
                + " vendor=" + System.getProperty("java.vm.specification.vendor")
                + " version=" + System.getProperty("java.vm.specification.version"));
        RuntimeMXBean rt = ManagementFactory.getRuntimeMXBean();
        System.out.println("runtime-bean name-same=" + same(rt.getVmName(), "java.vm.name")
                + " version-same=" + same(rt.getVmVersion(), "java.vm.version")
                + " vendor-same=" + same(rt.getVmVendor(), "java.vm.vendor"));
        System.out.println("runtime-bean spec-name-same="
                + same(rt.getSpecName(), "java.vm.specification.name")
                + " spec-vendor-same=" + same(rt.getSpecVendor(), "java.vm.specification.vendor")
                + " spec-version-same="
                + same(rt.getSpecVersion(), "java.vm.specification.version"));
        if (args.length == 1 && args[0].equals("wait")) {
            long until = System.currentTimeMillis() + 300_000;
            while (go == 0 && System.currentTimeMillis() < until) {
                Thread.sleep(10);
            }
        }
        System.out.println("done");
    }

    static boolean same(String answer, String key) {
        return Objects.equals(answer, System.getProperty(key));
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
            ReferenceType type = awaitClass("L1W118VmIdentityAnswers");
            String version = str(type, "javaVersion");
            String name = str(type, "vmName");
            String info = str(type, "vmInfo");
            System.out.println("  vm.version() == java.version: "
                    + Objects.equals(vm.version(), version));
            System.out.println("  vm.name() == java.vm.name: " + Objects.equals(vm.name(), name));
            // The description with the debuggee's own strings named.
            String shape = vm.description().replace(version, "<java.version>")
                    .replace(name, "<java.vm.name>").replace(info, "<java.vm.info>")
                    .replace('\n', '|');
            System.out.println("  description: " + shape);
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

        static String str(ReferenceType type, String field) {
            Value v = type.getValue(type.fieldByName(field));
            return v == null ? null : ((StringReference) v).value();
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
