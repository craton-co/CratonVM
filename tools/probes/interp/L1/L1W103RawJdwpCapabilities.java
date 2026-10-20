// Interpreter round i5 wave 103, lane L1: `VirtualMachine.Capabilities` (1/12)
// and `VirtualMachine.CapabilitiesNew` (1/17), every bit by name, from a raw
// JDWP client, so the runner (tools/jdi/run-jdi-conformance.sh) compares them
// with HotSpot's. JDI hides index 18 (`canUseSourceNameFilters` is decided by
// JDI from the JDWP version), so only a raw client sees all of them. Proves
// docs/internal/fixed-bugs/interpreter-i100-L1-jdwp-capabilities-new-stratum-and-source-name-filters-FIXED-20261016.md
// (indexes 14 and 18); the bits this server still answers false are
// recorded in docs/internal/fixed-bugs/interpreter-i103-L1-jdwp-capabilities-not-served-FIXED-20261016.md
// and listed in tools/jdi/known-differences.txt.
//
// Canonical transcript: no ids, no addresses, no timings.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   spun
//
// Under a debugger (the debuggee waits up to five minutes for the debugger to
// set the static `done`):
//
//   javac -g -d out L1W103RawJdwpCapabilities.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L1W103RawJdwpCapabilities wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5791 -cp out L1W103RawJdwpCapabilities wait)
//   java -cp out L1W103RawJdwpCapabilities debug 5791 > transcript.txt
//
// HotSpot 25 as the debuggee (MEASURED on build w103's Linux host run:
// `canUseSourceNameFilters=0`; the rest derived from VirtualMachineImpl.c
// `capabilities` / `capabilitiesNew`, and matching that run):
//
//   == attach
//   == Capabilities
//     canWatchFieldModification=1
//     canWatchFieldAccess=1
//     canGetBytecodes=1
//     canGetSyntheticAttribute=1
//     canGetOwnedMonitorInfo=1
//     canGetCurrentContendedMonitor=1
//     canGetMonitorInfo=1
//   == CapabilitiesNew
//     canWatchFieldModification=1
//     canWatchFieldAccess=1
//     canGetBytecodes=1
//     canGetSyntheticAttribute=1
//     canGetOwnedMonitorInfo=1
//     canGetCurrentContendedMonitor=1
//     canGetMonitorInfo=1
//     canRedefineClasses=1
//     canAddMethod=0
//     canUnrestrictedlyRedefineClasses=0
//     canPopFrames=1
//     canUseInstanceFilters=1
//     canGetSourceDebugExtension=1
//     canRequestVMDeathEvent=1
//     canSetDefaultStratum=1
//     canGetInstanceInfo=1
//     canRequestMonitorEvents=1
//     canGetMonitorFrameInfo=1
//     canUseSourceNameFilters=0
//     canGetConstantPool=1
//     canForceEarlyReturn=1
//     reserved=0
//   == done
//
// CratonVM on w103: `canGetMonitorInfo=0` (both blocks),
// `canRedefineClasses=0`, `canGetInstanceInfo=0`, `canRequestMonitorEvents=0`,
// `canGetConstantPool=0` and `canUseSourceNameFilters=1`. Since the w103
// hand-back (not built or run): only `canRedefineClasses=0` differs (the
// i103-L1 page above; allowed in tools/jdi/known-differences.txt), the
// others are served (L1W103JdiMonitorInfo, L1W103JdiInstances,
// L1W103JdiMonitorEvents) and index 18 is 0 again.
public class L1W103RawJdwpCapabilities {
    /** Set to 1 by the debugger to let `main` finish (`wait` mode). */
    static volatile int done;

    static final String[] OLD = {
        "canWatchFieldModification", "canWatchFieldAccess", "canGetBytecodes",
        "canGetSyntheticAttribute", "canGetOwnedMonitorInfo",
        "canGetCurrentContendedMonitor", "canGetMonitorInfo",
    };

    static final String[] NEW = {
        "canRedefineClasses", "canAddMethod", "canUnrestrictedlyRedefineClasses",
        "canPopFrames", "canUseInstanceFilters", "canGetSourceDebugExtension",
        "canRequestVMDeathEvent", "canSetDefaultStratum", "canGetInstanceInfo",
        "canRequestMonitorEvents", "canGetMonitorFrameInfo", "canUseSourceNameFilters",
        "canGetConstantPool", "canForceEarlyReturn",
    };

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean wait = args.length == 1 && args[0].equals("wait");
        long until = System.currentTimeMillis() + 300_000;
        while (wait && done == 0 && System.currentTimeMillis() < until) {
            Thread.sleep(20);
        }
        System.out.println("spun");
    }

    /** A minimal JDWP client (8-byte ids, as both VMs answer `IDSizes`). */
    static final class Debugger {
        static java.io.DataInputStream in;
        static java.io.DataOutputStream out;
        static int nextId = 1;

        static void run(int port) throws Exception {
            long deadline = System.currentTimeMillis() + 300_000;
            java.net.Socket socket = null;
            while (socket == null) {
                try {
                    socket = new java.net.Socket("127.0.0.1", port);
                } catch (java.io.IOException notYet) {
                    if (System.currentTimeMillis() > deadline) {
                        throw notYet;
                    }
                    Thread.sleep(250);
                }
            }
            socket.setSoTimeout(120_000);
            in = new java.io.DataInputStream(new java.io.BufferedInputStream(socket.getInputStream()));
            out = new java.io.DataOutputStream(socket.getOutputStream());
            byte[] hello = "JDWP-Handshake".getBytes(java.nio.charset.StandardCharsets.US_ASCII);
            out.write(hello);
            out.flush();
            byte[] back = new byte[hello.length];
            in.readFully(back);
            if (!java.util.Arrays.equals(hello, back)) {
                throw new IllegalStateException("no JDWP handshake");
            }
            System.out.println("== attach");

            java.io.DataInputStream r = command(1, 7, new byte[0]); // IDSizes
            for (int i = 0; i < 5; i++) {
                if (r.readInt() != 8) {
                    throw new IllegalStateException("ids are not 8 bytes");
                }
            }

            System.out.println("== Capabilities");
            r = command(1, 12, new byte[0]);
            for (String name : OLD) {
                System.out.println("  " + name + "=" + (r.readBoolean() ? 1 : 0));
            }
            System.out.println("== CapabilitiesNew");
            r = command(1, 17, new byte[0]);
            for (String name : OLD) {
                System.out.println("  " + name + "=" + (r.readBoolean() ? 1 : 0));
            }
            for (String name : NEW) {
                System.out.println("  " + name + "=" + (r.readBoolean() ? 1 : 0));
            }
            // The eleven reserved bits (22-32): one row, 1 if any is set.
            boolean reserved = false;
            for (int i = OLD.length + NEW.length; i < 32; i++) {
                reserved |= r.readBoolean();
            }
            System.out.println("  reserved=" + (reserved ? 1 : 0));

            long type = 0;
            while (type == 0) {
                Payload p = new Payload();
                p.string("LL1W103RawJdwpCapabilities;");
                r = command(1, 2, p.bytes()); // ClassesBySignature
                if (r.readInt() > 0) {
                    r.readByte();
                    type = r.readLong();
                } else if (System.currentTimeMillis() > deadline) {
                    throw new IllegalStateException("the probe class never loaded");
                } else {
                    Thread.sleep(50);
                }
            }
            long doneField = 0;
            r = command(2, 4, new Payload().id(type).bytes()); // Fields
            for (int n = r.readInt(); n > 0; n--) {
                long id = r.readLong();
                String name = readString(r);
                readString(r);
                r.readInt();
                if (name.equals("done")) {
                    doneField = id;
                }
            }
            Payload p = new Payload().id(type);
            p.i32(1);
            p.id(doneField);
            p.i32(1);
            command(3, 2, p.bytes()); // ClassType.SetValues
            System.out.println("== done");
            try {
                command(1, 6, new byte[0]); // Dispose
            } catch (java.io.IOException exited) {
                // The debuggee may exit and close the socket first.
            }
            socket.close();
        }

        /** Send a command and read its reply, skipping the VM's own packets. */
        static java.io.DataInputStream command(int set, int cmd, byte[] data) throws Exception {
            int id = nextId++;
            out.writeInt(11 + data.length);
            out.writeInt(id);
            out.writeByte(0);
            out.writeByte(set);
            out.writeByte(cmd);
            out.write(data);
            out.flush();
            while (true) {
                int length = in.readInt();
                int got = in.readInt();
                int flags = in.readUnsignedByte();
                if ((flags & 0x80) == 0) {
                    // A command from the VM (an event set): skip it.
                    in.readUnsignedShort();
                    in.readFully(new byte[length - 11]);
                    continue;
                }
                int error = in.readUnsignedShort();
                byte[] body = new byte[length - 11];
                in.readFully(body);
                if (got != id) {
                    continue;
                }
                if (error != 0) {
                    throw new IllegalStateException("JDWP error " + error + " for " + set + "/" + cmd);
                }
                return new java.io.DataInputStream(new java.io.ByteArrayInputStream(body));
            }
        }

        static String readString(java.io.DataInputStream r) throws java.io.IOException {
            byte[] b = new byte[r.readInt()];
            r.readFully(b);
            return new String(b, java.nio.charset.StandardCharsets.UTF_8);
        }

        static final class Payload {
            final java.io.ByteArrayOutputStream buf = new java.io.ByteArrayOutputStream();
            final java.io.DataOutputStream d = new java.io.DataOutputStream(buf);

            Payload id(long v) throws java.io.IOException {
                d.writeLong(v);
                return this;
            }

            void i32(int v) throws java.io.IOException {
                d.writeInt(v);
            }

            void string(String s) throws java.io.IOException {
                byte[] b = s.getBytes(java.nio.charset.StandardCharsets.UTF_8);
                d.writeInt(b.length);
                d.write(b);
            }

            byte[] bytes() {
                return buf.toByteArray();
            }
        }
    }
}
