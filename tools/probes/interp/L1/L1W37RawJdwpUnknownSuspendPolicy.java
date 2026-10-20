// Interpreter round i1 wave 37, lane L1: what a JDWP back end does with an
// event request whose suspend policy is not one of the three JDWP defines
// (0 NONE, 1 EVENT_THREAD, 2 ALL), for the last row of
// docs/internal/fixed-bugs/interpreter-L1-jdwp-event-request-filters-naming-no-thread-or-class-are-accepted-FIXED-20261001.md.
//
// A raw JDWP debugger (JDI never sends such a policy) sets a MethodEntry
// request filtered to the main thread and the probe class, with a Count of 1,
// once per policy (0, 1, 2, then 3, 7 and 255), lets `main` call `tick()`,
// and prints, per policy: the request's answer, the suspend policy the event
// set (Composite) carries, and the suspend counts of the event thread (main)
// and of another running thread right after the event. Each suspension is
// then undone (ThreadReference.Resume, as many times as counted).
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
//   javac -g -d out L1W37RawJdwpUnknownSuspendPolicy.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L1W37RawJdwpUnknownSuspendPolicy wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5791 -cp out L1W37RawJdwpUnknownSuspendPolicy wait)
//   java -cp out L1W37RawJdwpUnknownSuspendPolicy debug 5791 > transcript.txt
//
// HotSpot 25.0.3 as the debuggee (the debugger's stdout; three runs, the
// same each time):
//
//   == attach
//   == method entry of tick, count 1, per suspend policy
//     policy 0: ok, event set policy=0 main suspendCount=0 other suspendCount=0
//     policy 1: ok, event set policy=1 main suspendCount=1 other suspendCount=0
//     policy 2: ok, event set policy=2 main suspendCount=1 other suspendCount=1
//     policy 3: ok, event set policy=3 main suspendCount=1 other suspendCount=0
//     policy 7: ok, event set policy=7 main suspendCount=1 other suspendCount=0
//     policy 255: ok, event set policy=255 main suspendCount=1 other suspendCount=0
//   == done
//
// So HotSpot accepts every policy byte, carries it unchanged in the event
// set, and suspends the event thread alone for every value but 0 and 2.
//
// CratonVM before wave 37 (from reading `commands::handle_er_set`): the rows
// for 3, 7 and 255 printed `error 113` (`SuspendPolicy::from_u8` refused
// them). Since wave 37 (`SuspendPolicy::from_wire`) they are accepted and
// suspend the event thread alone, as on HotSpot; in wave 37 the event set
// carried 1 (`EVENT_THREAD`, the policy applied) where HotSpot carries the
// byte sent. Since wave 38 the request's byte is recorded beside it
// (`EventManager::note_wire_suspend_policy`) and the event set reports it
// (`debug::event_set_policy_byte`): every row matches HotSpot, and the
// scenario is in the JDI conformance runner's default list.
public class L1W37RawJdwpUnknownSuspendPolicy {
    /** Set to 1 by the debugger to let `main` finish (`wait` mode). */
    static volatile int done;
    static volatile int ticks;
    static long until;

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean wait = args.length == 1 && args[0].equals("wait");
        if (!wait) {
            done = 1;
        }
        until = System.currentTimeMillis() + 300_000;
        Thread other = new Thread(L1W37RawJdwpUnknownSuspendPolicy::otherSpin, "other");
        other.setDaemon(true);
        other.start();
        spin();
        System.out.println("spun");
    }

    static void tick() {
        ticks++;
    }

    static void spin() throws InterruptedException {
        while (done == 0 && System.currentTimeMillis() < until) {
            tick();
            Thread.sleep(20);
        }
    }

    static void otherSpin() {
        try {
            while (done == 0 && System.currentTimeMillis() < until) {
                Thread.sleep(20);
            }
        } catch (InterruptedException stop) {
            // Ends with the program.
        }
    }

    /** A minimal JDWP client (8-byte ids, as both VMs answer `IDSizes`). */
    static final class Debugger {
        static java.io.DataInputStream in;
        static java.io.DataOutputStream out;
        static int nextId = 1;
        /** Event sets (their data) read while waiting for a reply. */
        static final java.util.ArrayDeque<byte[]> events = new java.util.ArrayDeque<>();

        static final int METHOD_ENTRY = 40;

        static long type;
        static long main;
        static long other;

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
            while (type == 0) {
                Payload p = new Payload();
                p.string("LL1W37RawJdwpUnknownSuspendPolicy;");
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
            long doneField = 0;
            r = check(command(2, 4, new Payload().id(type).bytes())); // Fields
            for (int n = r.readInt(); n > 0; n--) {
                long id = r.readLong();
                String name = readString(r);
                readString(r);
                r.readInt();
                if (name.equals("done")) {
                    doneField = id;
                }
            }
            while (main == 0 || other == 0) {
                r = check(command(1, 4, new byte[0])); // AllThreads
                java.util.List<Long> threads = new java.util.ArrayList<>();
                for (int n = r.readInt(); n > 0; n--) {
                    threads.add(r.readLong());
                }
                for (long t : threads) {
                    java.io.DataInputStream name = check(command(11, 1, new Payload().id(t).bytes()));
                    String s = readString(name);
                    if (s.equals("main")) {
                        main = t;
                    } else if (s.equals("other")) {
                        other = t;
                    }
                }
                if (System.currentTimeMillis() > deadline) {
                    throw new IllegalStateException("setup incomplete");
                }
                if (main == 0 || other == 0) {
                    Thread.sleep(50);
                }
            }

            System.out.println("== method entry of tick, count 1, per suspend policy");
            for (int policy : new int[] {0, 1, 2, 3, 7, 255}) {
                policyRow(policy);
            }

            Payload p = new Payload().id(type);
            p.i32(1);
            p.id(doneField);
            p.i32(1);
            check(command(3, 2, p.bytes())); // ClassType.SetValues
            try {
                command(1, 6, new byte[0]); // Dispose
            } catch (java.io.IOException exited) {
                // The debuggee may exit and close the socket first.
            }
            System.out.println("== done");
            socket.close();
        }

        /** One row: set the request, await its event, read both suspend counts, undo. */
        static void policyRow(int policy) throws Exception {
            Payload p = new Payload();
            p.u8(METHOD_ENTRY);
            p.u8(policy);
            p.i32(3);
            // Count last: the back end applies the modifiers in order, and a
            // Count ahead of the filters would expire on another event.
            p.u8(3); // ThreadOnly
            p.id(main);
            p.u8(4); // ClassOnly
            p.id(type);
            p.u8(1); // Count
            p.i32(1);
            Reply reply = command(15, 1, p.bytes());
            if (reply.error != 0) {
                System.out.println("  policy " + policy + ": error " + reply.error);
                return;
            }
            int requestId = reply.data().readInt();
            byte[] set = awaitEventOf(requestId);
            if (set == null) {
                System.out.println("  policy " + policy + ": ok, no event");
                clear(requestId);
                return;
            }
            int carried = set[0] & 0xff;
            int mainCount = suspendCount(main);
            int otherCount = suspendCount(other);
            System.out.println("  policy " + policy + ": ok, event set policy=" + carried
                    + " main suspendCount=" + mainCount + " other suspendCount=" + otherCount);
            for (int i = 0; i < mainCount; i++) {
                check(command(11, 3, new Payload().id(main).bytes())); // ThreadReference.Resume
            }
            for (int i = 0; i < otherCount; i++) {
                check(command(11, 3, new Payload().id(other).bytes()));
            }
            // A Count-expired request is gone already; clearing it again is ok.
            clear(requestId);
        }

        static void clear(int requestId) throws Exception {
            Payload p = new Payload();
            p.u8(METHOD_ENTRY);
            p.i32(requestId);
            command(15, 2, p.bytes());
        }

        static int suspendCount(long thread) throws Exception {
            return check(command(11, 12, new Payload().id(thread).bytes())).readInt();
        }

        /** The data of the next event set holding an event of `requestId`, or null after 30 s. */
        static byte[] awaitEventOf(int requestId) throws Exception {
            long until = System.currentTimeMillis() + 30_000;
            while (System.currentTimeMillis() < until) {
                while (events.isEmpty()) {
                    readPacket(-1);
                }
                byte[] set = events.poll();
                java.io.DataInputStream d = new java.io.DataInputStream(
                        new java.io.ByteArrayInputStream(set));
                d.readUnsignedByte(); // suspendPolicy
                for (int n = d.readInt(); n > 0; n--) {
                    d.readUnsignedByte(); // eventKind
                    if (d.readInt() == requestId) {
                        return set;
                    }
                    break; // only the first event's request id is read
                }
            }
            return null;
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

        /** The reply [`readPacket`] read for the command it was asked about. */
        static Reply lastReply;

        /**
         * Read one packet. An event set (a Composite command from the VM) is
         * queued; a reply to `replyId` is kept in `lastReply`; another reply
         * is dropped.
         */
        static void readPacket(int replyId) throws Exception {
            int length = in.readInt();
            int got = in.readInt();
            int flags = in.readUnsignedByte();
            if ((flags & 0x80) == 0) {
                int set = in.readUnsignedByte();
                int cmd = in.readUnsignedByte();
                byte[] data = new byte[length - 11];
                in.readFully(data);
                if (set == 64 && cmd == 100) {
                    events.add(data);
                }
                return;
            }
            Reply reply = new Reply();
            reply.error = in.readUnsignedShort();
            reply.body = new byte[length - 11];
            in.readFully(reply.body);
            if (got == replyId) {
                lastReply = reply;
            }
        }

        /** Send a command and read its reply, queueing the VM's event sets. */
        static Reply command(int set, int cmd, byte[] data) throws Exception {
            int id = nextId++;
            out.writeInt(11 + data.length);
            out.writeInt(id);
            out.writeByte(0);
            out.writeByte(set);
            out.writeByte(cmd);
            out.write(data);
            out.flush();
            lastReply = null;
            while (lastReply == null) {
                readPacket(id);
            }
            return lastReply;
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

            void raw(byte[] b) throws java.io.IOException {
                d.write(b);
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
