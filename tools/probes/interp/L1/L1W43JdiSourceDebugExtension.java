// Interpreter round i1 wave 43, lane L1: `ReferenceType.SourceDebugExtension`
// (JDWP 2/12) and `canGetSourceDebugExtension`, stage 1 of
// docs/internal/fixed-bugs/interpreter-L1-proposal-pop-frames-force-early-return-and-source-debug-extension-FIXED-20261008.md.
//
// The debuggee waits (`go`), then writes a minimal class file,
// `L1W43SdeHolder`, into the first directory of its class path and loads it
// with `Class.forName`: a class with no members whose `SourceFile` is
// `Holder.kt` and whose `SourceDebugExtension` (JVMS 4.7.11) is a JSR-45
// SMAP with a `Kotlin` stratum of two files, `Holder.kt` and `Inline.kt`, as
// kotlinc writes one for a file that calls another file's inline function.
// Before `go` the debugger makes three ClassPrepare requests filtered by
// source name (the SourceFile's, a name only the SMAP lists, and one nobody
// lists) and prints which report the holder. It then asks for the SMAP of
// that class, of the probe class itself (no such attribute) and of
// `String[]` (an array class), and the strata JDI derives from it.
//
// Canonical transcript: no ids, no addresses, no timings.
// Run by the conformance runner, tools/jdi/run-jdi-conformance.sh
// (`--scenario L1W43JdiSourceDebugExtension`).
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   loaded L1W43SdeHolder
//
// Under a debugger (the debuggee waits up to five minutes):
//
//   javac -g -d out L1W43JdiSourceDebugExtension.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L1W43JdiSourceDebugExtension wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5791 -cp out L1W43JdiSourceDebugExtension wait)
//   java -cp out L1W43JdiSourceDebugExtension debug 5791 > transcript.txt     # HotSpot 25's java
//
// HotSpot 25.0.3 as the debuggee (the debugger's stdout; three runs, the
// same each time):
//
//   == attach
//   canGetSourceDebugExtension: true
//   == class prepare by source name
//     Holder.kt: reported
//     Inline.kt: reported
//     Nope.kt: not reported
//   == a class with an SMAP
//     sourceDebugExtension: SMAP|Holder.kt|Kotlin|*S Kotlin|*F|+ 1 Holder.kt|test/Holder.kt|+ 2 Inline.kt|test/Inline.kt|*L|1#1,10:1|1#2,5:20|*E|
//     defaultStratum: Kotlin
//     availableStrata: [Java, Kotlin]
//     sourceName: Holder.kt
//     sourceNames(Kotlin): [Holder.kt, Inline.kt]
//     sourcePaths(Kotlin): [test/Holder.kt, test/Inline.kt]
//   == a class without one
//     sourceDebugExtension: AbsentInformationException
//     defaultStratum: Java
//     availableStrata: [Java]
//   == an array class
//     sourceDebugExtension: AbsentInformationException
//   == end
//     disconnected
//
// CratonVM before wave 43 answered `canGetSourceDebugExtension` false
// (`commands::handle_vm_capabilities_new`), so JDI threw
// `UnsupportedOperationException` from `sourceDebugExtension()` itself and
// ignored the SMAP everywhere else (`defaultStratum: Java`, `availableStrata:
// [Java]` for the holder too); 2/12 answered `NOT_IMPLEMENTED`; and a
// `SourceNameMatch` matched the `SourceFile` only (`Inline.kt: not
// reported`). Since wave 43 the server reads the attribute from the class
// file the class was defined from (`debug::source_debug_extension`), and a
// ClassPrepare source-name filter matches the SMAP's file names too
// (`debug::smap_source_names`). Every CratonVM mode must print the same.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.util.*;

public class L1W43JdiSourceDebugExtension {
    /** Set to 1 by the debugger to let `main` load the holder (`wait` mode). */
    static volatile int go;
    /** Set to 1 by the debugger to let `main` finish (`wait` mode). */
    static volatile int done;

    /** Two source files, as kotlinc writes for a file that calls another's inline function. */
    static final String SMAP = "SMAP\nHolder.kt\nKotlin\n*S Kotlin\n*F\n+ 1 Holder.kt\n"
            + "test/Holder.kt\n+ 2 Inline.kt\ntest/Inline.kt\n*L\n1#1,10:1\n1#2,5:20\n*E\n";

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean wait = args.length == 1 && args[0].equals("wait");
        long until = System.currentTimeMillis() + 300_000;
        while (wait && go == 0 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        String dir = System.getProperty("java.class.path").split(java.io.File.pathSeparator)[0];
        java.nio.file.Files.write(java.nio.file.Path.of(dir, "L1W43SdeHolder.class"), holder());
        Class<?> c = Class.forName("L1W43SdeHolder");
        while (wait && done == 0 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        System.out.println("loaded " + c.getName());
    }

    /**
     * `public class L1W43SdeHolder extends Object` with no members, a
     * `SourceFile` of `Holder.kt` and a `SourceDebugExtension` of [SMAP].
     */
    static byte[] holder() throws Exception {
        java.io.ByteArrayOutputStream buf = new java.io.ByteArrayOutputStream();
        java.io.DataOutputStream d = new java.io.DataOutputStream(buf);
        d.writeInt(0xCAFEBABE);
        d.writeShort(0);
        d.writeShort(52); // any version the VM reads; no stack maps needed
        String[] utf8 = {"L1W43SdeHolder", "java/lang/Object", "SourceFile", "Holder.kt",
                "SourceDebugExtension"};
        // #1 Utf8 this, #2 Class #1, #3 Utf8 super, #4 Class #3, #5..#7 Utf8.
        d.writeShort(8);
        d.writeByte(1);
        d.writeUTF(utf8[0]);
        d.writeByte(7);
        d.writeShort(1);
        d.writeByte(1);
        d.writeUTF(utf8[1]);
        d.writeByte(7);
        d.writeShort(3);
        for (int i = 2; i < utf8.length; i++) {
            d.writeByte(1);
            d.writeUTF(utf8[i]);
        }
        d.writeShort(0x0021); // ACC_PUBLIC | ACC_SUPER
        d.writeShort(2);
        d.writeShort(4);
        d.writeShort(0); // interfaces
        d.writeShort(0); // fields
        d.writeShort(0); // methods
        d.writeShort(2); // attributes
        d.writeShort(5); // SourceFile
        d.writeInt(2);
        d.writeShort(6);
        byte[] smap = SMAP.getBytes(java.nio.charset.StandardCharsets.UTF_8);
        d.writeShort(7); // SourceDebugExtension
        d.writeInt(smap.length);
        d.write(smap);
        d.flush();
        return buf.toByteArray();
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
            System.out.println("canGetSourceDebugExtension: " + vm.canGetSourceDebugExtension());
            ReferenceType probe = awaitClass("L1W43JdiSourceDebugExtension");

            // ClassPrepare requests filtered by source name, before the
            // holder loads: the SourceFile's name, a name only the SMAP
            // lists, and one nobody lists.
            System.out.println("== class prepare by source name");
            List<String> filters = List.of("Holder.kt", "Inline.kt", "Nope.kt");
            com.sun.jdi.request.EventRequestManager erm = vm.eventRequestManager();
            for (String f : filters) {
                com.sun.jdi.request.ClassPrepareRequest r = erm.createClassPrepareRequest();
                r.addSourceNameFilter(f);
                r.setSuspendPolicy(com.sun.jdi.request.EventRequest.SUSPEND_NONE);
                r.putProperty("filter", f);
                r.enable();
            }
            ((ClassType) probe).setValue(probe.fieldByName("go"), vm.mirrorOf(1));
            ReferenceType holder = awaitClass("L1W43SdeHolder");
            Set<String> reported = new TreeSet<>();
            long quiet = System.currentTimeMillis() + 3_000;
            while (System.currentTimeMillis() < quiet) {
                EventSet set = vm.eventQueue().remove(500);
                if (set == null) {
                    continue;
                }
                for (Event e : set) {
                    if (e instanceof ClassPrepareEvent p
                            && p.referenceType().name().equals("L1W43SdeHolder")) {
                        reported.add((String) p.request().getProperty("filter"));
                    }
                }
                set.resume();
            }
            for (String f : filters) {
                System.out.println("  " + f + ": " + (reported.contains(f) ? "reported" : "not reported"));
            }

            System.out.println("== a class with an SMAP");
            System.out.println("  sourceDebugExtension: " + sde(holder));
            System.out.println("  defaultStratum: " + holder.defaultStratum());
            System.out.println("  availableStrata: " + sorted(holder.availableStrata()));
            System.out.println("  sourceName: " + sourceName(holder));
            try {
                System.out.println("  sourceNames(Kotlin): " + holder.sourceNames("Kotlin"));
                // JDI joins a path with the debugger's own separator.
                System.out.println("  sourcePaths(Kotlin): "
                        + holder.sourcePaths("Kotlin").toString().replace('\\', '/'));
            } catch (AbsentInformationException none) {
                System.out.println("  sourceNames(Kotlin): AbsentInformationException");
            }

            System.out.println("== a class without one");
            System.out.println("  sourceDebugExtension: " + sde(probe));
            System.out.println("  defaultStratum: " + probe.defaultStratum());
            System.out.println("  availableStrata: " + sorted(probe.availableStrata()));

            System.out.println("== an array class");
            List<ReferenceType> arrays = vm.classesByName("java.lang.String[]");
            System.out.println("  sourceDebugExtension: "
                    + (arrays.isEmpty() ? "no String[]" : sde(arrays.get(0))));

            System.out.println("== end");
            ((ClassType) probe).setValue(probe.fieldByName("done"), vm.mirrorOf(1));
            boolean ended = false;
            while (!ended) {
                EventSet set;
                try {
                    set = vm.eventQueue().remove(60_000);
                } catch (VMDisconnectedException gone) {
                    break;
                }
                if (set == null) {
                    fail("the debuggee did not end");
                }
                for (Event e : set) {
                    if (e instanceof VMDisconnectEvent) {
                        ended = true;
                    }
                }
                if (!ended) {
                    set.resume();
                }
            }
            System.out.println("  disconnected");
        }

        /** The SMAP with its line breaks shown as `|`, or the exception's name. */
        static String sde(ReferenceType type) {
            try {
                return type.sourceDebugExtension().replace('\n', '|');
            } catch (AbsentInformationException | UnsupportedOperationException e) {
                return e.getClass().getSimpleName();
            }
        }

        static String sourceName(ReferenceType type) {
            try {
                return type.sourceName();
            } catch (AbsentInformationException e) {
                return "AbsentInformationException";
            }
        }

        static List<String> sorted(List<String> strata) {
            List<String> s = new ArrayList<>(strata);
            Collections.sort(s);
            return s;
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
