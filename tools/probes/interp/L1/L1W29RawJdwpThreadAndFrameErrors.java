// Interpreter round i1 wave 29, lane L1: a raw JDWP scenario for the error
// answers of the `ThreadReference` and `StackFrame` commands (review of
// `vm/src/debug/commands.rs` and `vm/src/debug/inspect.rs`).
//
// JDI checks a thread's liveness and a slot's type before it asks, so a JDI
// debugger rarely sees these answers; any other JDWP client does. This
// debugger speaks JDWP over a raw socket and prints one row per command:
//
//  * each ThreadReference command on a thread that has TERMINATED (its id
//    taken while it was alive);
//  * the frame commands on a live thread that is NOT suspended;
//  * StackFrame.GetValues of `probe(int i, long l, String s, boolean z)`'s
//    frame, suspended, with right and wrong slots and tags.
//
// Canonical transcript: no ids, no addresses, no timings.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   probed 7
//
// Under a debugger (the debuggee waits up to five minutes for the debugger to
// set the static `phase`):
//
//   javac -g -d out L1W29RawJdwpThreadAndFrameErrors.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L1W29RawJdwpThreadAndFrameErrors wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5791 -cp out L1W29RawJdwpThreadAndFrameErrors wait)
//   java -cp out L1W29RawJdwpThreadAndFrameErrors debug 5791 > transcript.txt
//
// HotSpot 25.0.3 as the debuggee (the debugger's stdout):
//
//   == attach
//   == a terminated thread
//     Name: ok
//     Status: ok 0 0
//     Suspend: ok
//     SuspendCount: ok 1
//     Resume: ok
//     Frames: error 13
//     FrameCount: error 13
//     ThreadGroup: ok null=true
//     Interrupt: error 10
//     OwnedMonitors: error 13
//     CurrentContendedMonitor: error 13
//   == a live thread not suspended
//     Frames: error 13
//     FrameCount: error 13
//     OwnedMonitors: error 13
//     CurrentContendedMonitor: error 13
//     SuspendCount: ok 0
//     Resume: ok
//     SuspendCount after Resume: ok 0
//   == the frame of probe(int, long, String, boolean)
//     i as I: ok I 3
//     i as J: error 34
//     i as F: error 34
//     i as L: error 34
//     i as Z: ok Z true
//     l as J: ok J 4
//     l as I: error 34
//     l as D: error 34
//     s as L: ok s object
//     s as I: error 34
//     s as [: ok s object
//     z as Z: ok Z true
//     z as I: ok I 1
//     slot 99 as I: error 35
//     i as B: ok B 3
//     i as S: ok S 3
//     s as s: ok s object
//     s as t: ok s object
//     s as c: ok s object
//     s as g: ok s object
//     s as l: ok s object
//     i as s: error 34
//     i as V: error 500
//     slot 0 as tag 0: error 500
//     slot 0 as tag X: error 500
//     ThisObject: ok tag=L
//     GetValues of an unknown frame id: error 30
//   == done
//
// CratonVM before wave 29 (from reading `commands::sf_get_values_tagged`):
// `s as s` / `t` / `c` / `g` / `l` answered `ok V void` and `i as s`
// answered `ok V void` too; `i as V` and the two unknown tags answered
// `ok V void` where HotSpot answers 500 (INVALID_TAG). Wave 29 answers them
// as HotSpot does, and counts the suspension of the terminated thread
// (`SuspendCount: ok 1`) without letting it hold the live threads in the
// interpreter.
public class L1W29RawJdwpThreadAndFrameErrors {
    /** 1: the short-lived thread may end; 2: `probe` may return. */
    static volatile int phase;
    static volatile boolean shortStarted;
    static long until;

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean wait = args.length == 1 && args[0].equals("wait");
        if (!wait) {
            phase = 2;
        }
        until = System.currentTimeMillis() + 300_000;
        Thread shortLived = new Thread(() -> {
            shortStarted = true;
            while (phase < 1 && System.currentTimeMillis() < until) {
                pause();
            }
        }, "short-lived");
        shortLived.start();
        Thread spinner = new Thread(() -> {
            while (phase < 2 && System.currentTimeMillis() < until) {
                pause();
            }
        }, "spinner");
        spinner.setDaemon(true);
        spinner.start();
        int r = probe(3, 4L, "s", true);
        shortLived.join();
        System.out.println("probed " + r);
    }

    static void pause() {
        try {
            Thread.sleep(20);
        } catch (InterruptedException e) {
            // Keep waiting.
        }
    }

    static int probe(int i, long l, String s, boolean z) throws InterruptedException {
        while (phase < 2 && System.currentTimeMillis() < until) {
            Thread.sleep(20);
        }
        return i + (int) l;
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
            java.io.DataInputStream r = check(command(1, 7, new byte[0])); // IDSizes
            for (int i = 0; i < 5; i++) {
                if (r.readInt() != 8) {
                    throw new IllegalStateException("ids are not 8 bytes");
                }
            }
            long type = 0;
            while (type == 0) {
                Payload p = new Payload();
                p.string("LL1W29RawJdwpThreadAndFrameErrors;");
                r = check(command(1, 2, p.bytes())); // ClassesBySignature
                if (r.readInt() > 0) {
                    r.readByte();
                    type = r.readLong();
                } else if (System.currentTimeMillis() > deadline) {
                    throw new IllegalStateException("the probe class never loaded");
                } else {
                    Thread.sleep(50);
                }
            }
            java.util.Map<String, Long> fields = new java.util.HashMap<>();
            r = check(command(2, 4, new Payload().id(type).bytes())); // Fields
            for (int n = r.readInt(); n > 0; n--) {
                long id = r.readLong();
                String name = readString(r);
                readString(r);
                r.readInt();
                fields.put(name, id);
            }
            long probe = 0;
            r = check(command(2, 5, new Payload().id(type).bytes())); // Methods
            for (int n = r.readInt(); n > 0; n--) {
                long id = r.readLong();
                String name = readString(r);
                readString(r);
                r.readInt();
                if (name.equals("probe")) {
                    probe = id;
                }
            }

            // The three threads, once the short-lived one runs.
            long main = 0;
            long shortLived = 0;
            long spinner = 0;
            while (main == 0 || shortLived == 0 || spinner == 0) {
                r = check(command(1, 4, new byte[0])); // AllThreads
                java.util.List<Long> threads = new java.util.ArrayList<>();
                for (int n = r.readInt(); n > 0; n--) {
                    threads.add(r.readLong());
                }
                for (long t : threads) {
                    Reply name = command(11, 1, new Payload().id(t).bytes());
                    if (name.error != 0) {
                        continue;
                    }
                    switch (readString(name.data())) {
                        case "main" -> main = t;
                        case "short-lived" -> shortLived = t;
                        case "spinner" -> spinner = t;
                        default -> { }
                    }
                }
                if (System.currentTimeMillis() > deadline) {
                    throw new IllegalStateException("threads never listed");
                }
                Thread.sleep(50);
            }

            // Let the short-lived thread end, and wait until it has.
            setStatic(type, fields.get("phase"), 1);
            while (true) {
                Reply status = command(11, 4, new Payload().id(shortLived).bytes());
                if (status.error != 0 || status.data().readInt() == 0) { // ZOMBIE
                    break;
                }
                if (System.currentTimeMillis() > deadline) {
                    throw new IllegalStateException("the short-lived thread never ended");
                }
                Thread.sleep(50);
            }

            System.out.println("== a terminated thread");
            show("Name", command(11, 1, new Payload().id(shortLived).bytes()), false);
            Reply status = command(11, 4, new Payload().id(shortLived).bytes());
            if (status.error == 0) {
                java.io.DataInputStream d = status.data();
                System.out.println("  Status: ok " + d.readInt() + " " + d.readInt());
            } else {
                System.out.println("  Status: error " + status.error);
            }
            show("Suspend", command(11, 2, new Payload().id(shortLived).bytes()), false);
            show("SuspendCount", command(11, 12, new Payload().id(shortLived).bytes()), true);
            show("Resume", command(11, 3, new Payload().id(shortLived).bytes()), false);
            show("Frames", command(11, 6, framesPayload(shortLived)), false);
            show("FrameCount", command(11, 7, new Payload().id(shortLived).bytes()), true);
            Reply group = command(11, 5, new Payload().id(shortLived).bytes());
            System.out.println("  ThreadGroup: " + (group.error != 0 ? "error " + group.error
                    : "ok null=" + (group.data().readLong() == 0)));
            show("Interrupt", command(11, 11, new Payload().id(shortLived).bytes()), false);
            show("OwnedMonitors", command(11, 8, new Payload().id(shortLived).bytes()), false);
            show("CurrentContendedMonitor",
                    command(11, 9, new Payload().id(shortLived).bytes()), false);

            System.out.println("== a live thread not suspended");
            show("Frames", command(11, 6, framesPayload(spinner)), false);
            show("FrameCount", command(11, 7, new Payload().id(spinner).bytes()), false);
            show("OwnedMonitors", command(11, 8, new Payload().id(spinner).bytes()), false);
            show("CurrentContendedMonitor",
                    command(11, 9, new Payload().id(spinner).bytes()), false);
            show("SuspendCount", command(11, 12, new Payload().id(spinner).bytes()), true);
            show("Resume", command(11, 3, new Payload().id(spinner).bytes()), false);
            show("SuspendCount after Resume",
                    command(11, 12, new Payload().id(spinner).bytes()), true);

            System.out.println("== the frame of probe(int, long, String, boolean)");
            long frame = 0;
            while (frame == 0) {
                check(command(11, 2, new Payload().id(main).bytes())); // Suspend
                r = check(command(11, 6, framesPayload(main)));
                for (int n = r.readInt(); n > 0; n--) {
                    long id = r.readLong();
                    r.readByte();
                    r.readLong();
                    long method = r.readLong();
                    r.readLong();
                    if (method == probe && frame == 0) {
                        frame = id;
                    }
                }
                if (frame == 0) {
                    check(command(11, 3, new Payload().id(main).bytes())); // Resume
                    if (System.currentTimeMillis() > deadline) {
                        throw new IllegalStateException("never in probe");
                    }
                    Thread.sleep(50);
                }
            }
            get(main, frame, "i as I", 0, 'I');
            get(main, frame, "i as J", 0, 'J');
            get(main, frame, "i as F", 0, 'F');
            get(main, frame, "i as L", 0, 'L');
            get(main, frame, "i as Z", 0, 'Z');
            get(main, frame, "l as J", 1, 'J');
            get(main, frame, "l as I", 1, 'I');
            get(main, frame, "l as D", 1, 'D');
            get(main, frame, "s as L", 3, 'L');
            get(main, frame, "s as I", 3, 'I');
            get(main, frame, "s as [", 3, '[');
            get(main, frame, "z as Z", 4, 'Z');
            get(main, frame, "z as I", 4, 'I');
            get(main, frame, "slot 99 as I", 99, 'I');
            get(main, frame, "i as B", 0, 'B');
            get(main, frame, "i as S", 0, 'S');
            get(main, frame, "s as s", 3, 's');
            get(main, frame, "s as t", 3, 't');
            get(main, frame, "s as c", 3, 'c');
            get(main, frame, "s as g", 3, 'g');
            get(main, frame, "s as l", 3, 'l');
            get(main, frame, "i as s", 0, 's');
            get(main, frame, "i as V", 0, 'V');
            get(main, frame, "slot 0 as tag 0", 0, 0);
            get(main, frame, "slot 0 as tag X", 0, 'X');
            Reply self = command(16, 3, new Payload().id(main).id(frame).bytes()); // ThisObject
            System.out.println("  ThisObject: " + (self.error != 0 ? "error " + self.error
                    : "ok tag=" + (char) self.data().readByte()));
            Reply bogus = command(16, 1, getPayload(main, frame ^ 0x5A5A_0000L, 0, 'I'));
            System.out.println("  GetValues of an unknown frame id: "
                    + (bogus.error != 0 ? "error " + bogus.error : "ok"));

            setStatic(type, fields.get("phase"), 2);
            check(command(1, 9, new byte[0])); // VirtualMachine.Resume
            try {
                command(1, 6, new byte[0]); // Dispose
            } catch (java.io.IOException exited) {
                // The debuggee may exit and close the socket first.
            }
            System.out.println("== done");
            socket.close();
        }

        static byte[] framesPayload(long thread) throws Exception {
            Payload p = new Payload().id(thread);
            p.i32(0);
            p.i32(-1);
            return p.bytes();
        }

        static byte[] getPayload(long thread, long frame, int slot, int tag) throws Exception {
            Payload p = new Payload().id(thread).id(frame);
            p.i32(1);
            p.i32(slot);
            p.u8(tag);
            return p.bytes();
        }

        /** `StackFrame.GetValues` of one slot: the error, or the value's tag. */
        static void get(long thread, long frame, String what, int slot, int tag) throws Exception {
            Reply reply = command(16, 1, getPayload(thread, frame, slot, tag));
            if (reply.error != 0) {
                System.out.println("  " + what + ": error " + reply.error);
                return;
            }
            java.io.DataInputStream d = reply.data();
            d.readInt();
            char got = (char) d.readByte();
            String value = switch (got) {
                case 'I' -> Integer.toString(d.readInt());
                case 'Z' -> Boolean.toString(d.readBoolean());
                case 'B' -> Byte.toString(d.readByte());
                case 'S' -> Short.toString(d.readShort());
                case 'C' -> Integer.toString(d.readChar());
                case 'V' -> "void";
                case 'J' -> Long.toString(d.readLong());
                case 'F' -> Float.toString(d.readFloat());
                case 'D' -> Double.toString(d.readDouble());
                default -> d.readLong() == 0 ? "null" : "object";
            };
            System.out.println("  " + what + ": ok " + got + " " + value);
        }

        /** One row: the error, or ok (with the int the reply starts with). */
        static void show(String what, Reply reply, boolean withInt) throws Exception {
            if (reply.error != 0) {
                System.out.println("  " + what + ": error " + reply.error);
            } else if (withInt) {
                System.out.println("  " + what + ": ok " + reply.data().readInt());
            } else {
                System.out.println("  " + what + ": ok");
            }
        }

        static void setStatic(long type, long field, int value) throws Exception {
            Payload p = new Payload().id(type);
            p.i32(1);
            p.id(field);
            p.i32(value);
            check(command(3, 2, p.bytes())); // ClassType.SetValues
        }

        static final class Reply {
            int error;
            byte[] body;

            java.io.DataInputStream data() {
                return new java.io.DataInputStream(new java.io.ByteArrayInputStream(body));
            }
        }

        static java.io.DataInputStream check(Reply reply) {
            if (reply.error != 0) {
                throw new IllegalStateException("JDWP error " + reply.error);
            }
            return reply.data();
        }

        /** Send a command and read its reply, skipping the VM's own packets. */
        static Reply command(int set, int cmd, byte[] data) throws Exception {
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
                Reply reply = new Reply();
                reply.error = in.readUnsignedShort();
                reply.body = new byte[length - 11];
                in.readFully(reply.body);
                if (got == id) {
                    return reply;
                }
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

            void u8(int v) throws java.io.IOException {
                d.writeByte(v);
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
