// Interpreter round i5 wave 104, lane L1: JDWP `VirtualMachine.RedefineClasses`
// (`canRedefineClasses`), through JDI.
//
// HotSpot's back end answers `canRedefineClasses` true (and `canAddMethod`,
// `canUnrestrictedlyRedefineClasses` false) and serves the command with JVMTI
// `RedefineClasses`, mapping a refusal to its JDWP error, which JDI turns into
// an exception (`SCHEMA_CHANGE_NOT_IMPLEMENTED`: an
// `UnsupportedOperationException`). CratonVM answered the bit false and did
// not serve the command; since wave 104 it serves it with the C JVMTI table's
// redefinition (vm/src/debug/mod.rs `redefine_classes`). Not built or run.
// Page: docs/internal/fixed-bugs/interpreter-i103-L1-jdwp-capabilities-not-served-FIXED-20261016.md
//
// The debugger redefines `Target` twice from its own class file: once with
// `value()` returning 42 instead of 41 (a method body change: accepted), then
// with a static field added (a schema change: refused). The debuggee calls
// `value()` before and after, and the second call must run the new body.
//
// Canonical transcript: no ids, no addresses, no timings.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   before=41 after=41
//
// Under a debugger (tools/jdi/run-jdi-conformance.sh runs it):
//
//   javac -g -d out L1W104JdiRedefineClasses.java
//   cratonvm --java-home $JDK --jdwp-port 5798 -cp out L1W104JdiRedefineClasses wait
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5798 \
//       -cp out L1W104JdiRedefineClasses wait
//   java -cp out L1W104JdiRedefineClasses debug 5798 > transcript.txt
//
// HotSpot 25 as the debuggee (derived from VirtualMachineImpl.c and JDI's
// VirtualMachineImpl.redefineClasses, NOT measured):
//
//   == attach
//     canRedefineClasses=true canAddMethod=false canUnrestrictedlyRedefineClasses=false
//   == before
//     before=41
//   == redefine the body
//     redefined
//   == after
//     after=42
//   == redefine with a field added
//     refused java.lang.UnsupportedOperationException
//   == end
//     disconnected
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.io.*;
import java.util.*;

public class L1W104JdiRedefineClasses {
    static final class Target {
        static int count;

        static int value() {
            count++;
            return 41;
        }
    }

    /** Set by the debugger: 1 = call before, 3 = call after, 5 = done. 2 and 4 are acks. */
    static volatile int phase;
    static int before;
    static int after;

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
        before = Target.value();
        if (debugged) {
            phase = 2;
        }
        while (debugged && phase != 3 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        after = Target.value();
        if (debugged) {
            phase = 4;
        }
        while (debugged && phase != 5 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        System.out.println("before=" + before + " after=" + after);
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

        static int intField(String name) {
            return ((IntegerValue) type.getValue(type.fieldByName(name))).value();
        }

        /** `Target`'s class file, as javac wrote it next to this class. */
        static byte[] targetBytes() throws IOException {
            String resource = "L1W104JdiRedefineClasses$Target.class";
            try (InputStream in = L1W104JdiRedefineClasses.class.getResourceAsStream(resource)) {
                if (in == null) {
                    fail("no " + resource);
                }
                return in.readAllBytes();
            }
        }

        /** `bytes` with `value()`'s `bipush 41; ireturn` made `bipush 42; ireturn`. */
        static byte[] withValue42(byte[] bytes) {
            byte[] out = bytes.clone();
            int found = -1;
            for (int i = 0; i + 2 < out.length; i++) {
                if (out[i] == 0x10 && out[i + 1] == 41 && (out[i + 2] & 0xff) == 0xac) {
                    if (found >= 0) {
                        fail("bipush 41; ireturn is not unique");
                    }
                    found = i;
                }
            }
            if (found < 0) {
                fail("no bipush 41; ireturn");
            }
            out[found + 1] = 42;
            return out;
        }

        /**
         * `bytes` with one more static field, `int value` (a name and a
         * descriptor the pool already holds: the method `value`, the field
         * `count`'s `I`).
         */
        static byte[] withFieldAdded(byte[] bytes) throws IOException {
            DataInputStream in = new DataInputStream(new ByteArrayInputStream(bytes));
            in.readInt(); // magic
            in.readUnsignedShort(); // minor
            in.readUnsignedShort(); // major
            int count = in.readUnsignedShort();
            int nameIndex = -1;
            int descriptorIndex = -1;
            for (int i = 1; i < count; i++) {
                int tag = in.readUnsignedByte();
                switch (tag) {
                    case 1 -> {
                        String s = in.readUTF();
                        if (s.equals("value")) {
                            nameIndex = i;
                        }
                        if (s.equals("I")) {
                            descriptorIndex = i;
                        }
                    }
                    case 3, 4 -> in.readInt();
                    case 5, 6 -> {
                        in.readLong();
                        i++;
                    }
                    case 7, 8, 16, 19, 20 -> in.readUnsignedShort();
                    case 9, 10, 11, 12, 17, 18 -> in.readInt();
                    case 15 -> {
                        in.readUnsignedByte();
                        in.readUnsignedShort();
                    }
                    default -> fail("constant tag " + tag);
                }
            }
            if (nameIndex < 0 || descriptorIndex < 0) {
                fail("no value / I in the pool");
            }
            in.readUnsignedShort(); // access
            in.readUnsignedShort(); // this
            in.readUnsignedShort(); // super
            int interfaces = in.readUnsignedShort();
            for (int i = 0; i < interfaces; i++) {
                in.readUnsignedShort();
            }
            int fieldsAt = bytes.length - in.available();
            int fields = ((bytes[fieldsAt] & 0xff) << 8) | (bytes[fieldsAt + 1] & 0xff);
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            out.write(bytes, 0, fieldsAt);
            DataOutputStream data = new DataOutputStream(out);
            data.writeShort(fields + 1);
            data.writeShort(0x0008); // ACC_STATIC
            data.writeShort(nameIndex);
            data.writeShort(descriptorIndex);
            data.writeShort(0); // attributes_count
            data.flush();
            out.write(bytes, fieldsAt + 2, bytes.length - fieldsAt - 2);
            return out.toByteArray();
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
            System.out.println("  canRedefineClasses=" + vm.canRedefineClasses()
                    + " canAddMethod=" + vm.canAddMethod()
                    + " canUnrestrictedlyRedefineClasses=" + vm.canUnrestrictedlyRedefineClasses());
            type = (ClassType) awaitClass("L1W104JdiRedefineClasses");
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(1));
            await(2);
            System.out.println("== before");
            System.out.println("  before=" + intField("before"));
            ReferenceType target = awaitClass("L1W104JdiRedefineClasses$Target");
            byte[] original = targetBytes();
            System.out.println("== redefine the body");
            if (vm.canRedefineClasses()) {
                try {
                    vm.redefineClasses(Map.of(target, withValue42(original)));
                    System.out.println("  redefined");
                } catch (RuntimeException | LinkageError e) {
                    System.out.println("  refused " + e.getClass().getName());
                }
            } else {
                System.out.println("  not supported");
            }
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(3));
            await(4);
            System.out.println("== after");
            System.out.println("  after=" + intField("after"));
            System.out.println("== redefine with a field added");
            if (vm.canRedefineClasses()) {
                try {
                    vm.redefineClasses(Map.of(target, withFieldAdded(withValue42(original))));
                    System.out.println("  redefined");
                } catch (RuntimeException | LinkageError e) {
                    System.out.println("  refused " + e.getClass().getName());
                }
            } else {
                System.out.println("  not supported");
            }
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
