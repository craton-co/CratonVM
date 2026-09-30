// Interpreter round i1 wave 42, lane L1: the answers, and the error codes, of
// the JDWP ThreadReference, StackFrame and ReferenceType commands at their
// edges, from a raw JDWP client (JDI checks most of these arguments itself
// and never sends them, but a raw client, an IDE's own protocol layer or a
// fuzzer does). The EventRequest edges are L1W29RawJdwpEventRequestErrors'.
//
// The debuggee's `main` waits for the debugger to set the static `go`, then
// calls `probe(3, "s")`, where a breakpoint (suspend policy EVENT_THREAD)
// stops it at bytecode index 0: `main` is then suspended with the frames
// `probe` and `main`. A daemon thread `other` sleeps in a loop, never
// suspended. One line per command, `<command>(<case>): <answer>`, where the
// answer is `error <code>` or the reply's gist.
//
// Canonical transcript: no ids, no addresses, no timings.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   probed
//
// Under a debugger (the debuggee waits up to five minutes for `go`):
//
//   javac -g -d out L1W42RawJdwpErrorAnswers.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L1W42RawJdwpErrorAnswers wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5791 -cp out L1W42RawJdwpErrorAnswers wait)
//   java -cp out L1W42RawJdwpErrorAnswers debug 5791 > transcript.txt
//
// HotSpot 25.0.3 as the debuggee (the debugger's stdout, three runs alike):
//
//   == attach
//   == main stopped in probe
//   == ThreadReference
//     frameCount(running other): error 13
//     frames(running other): error 13
//     ownedMonitors(running other): error 13
//     frameCount(main): 2
//     frames(main 0 -1): 2
//     frames(main 1 1): 1
//     frames(main 2 -1): 0
//     frames(main 2 0): 0
//     frames(main 3 -1): error 503
//     frames(main 3 0): 0
//     frames(main 0 3): error 504
//     frames(main 1 2): error 504
//     frames(main -1 1): error 503
//     frames(main 0 -2): error 504
//     name(unknown id): error 20
//     status(a class id): error 10
//     suspendCount(other): 0
//     resume(other, not suspended): ok
//     suspendCount(main): 1
//     isVirtual(main): false
//   == StackFrame
//     getValues(slot 0 I): I 3
//     getValues(slot 1 L): s
//     getValues(slot 0 J): error 35
//     getValues(slot 0 D): error 35
//     getValues(slot 1 I): error 34
//     getValues(slot 1 J): error 34
//     getValues(slot 2 L): error 34
//     getValues(slot 2 I): error 35
//     getValues(slot 9 I): error 35
//     getValues(unknown frame): error 30
//     thisObject(static frame): L null
//   == ReferenceType
//     signature(unknown id): error 20
//     signature(a thread id): error 21
//     status(probe class): 7
//     classFileVersion(probe class): 69.0
//     referenceType(String[]): tag 3
//     sourceFile(String[]): error 101
//     status(String[]): 0
//     modifiers(String[]): 0x431
//     methods(String[]): 0
//     fields(String[]): 0
//     interfaces(String[]): 0
//     classFileVersion(String[]): error 101
//     referenceType(int[]): tag 3
//     sourceFile(int[]): error 101
//     status(int[]): 0
//     modifiers(int[]): 0x431
//     methods(int[]): 0
//     fields(int[]): 0
//     interfaces(int[]): 0
//     classFileVersion(int[]): error 101
//   == done
//
// (Measured on the Windows box; the orchestrator should take the host's
// HotSpot run as the reference.) `probe`'s slot 2 is `t`, not yet in scope
// at bytecode 0 and never written.
//
// CratonVM before wave 42, from reading `debug::commands`: `frames(main 0 3)`
// and `frames(main 1 2)` answered 503 and `frames(main 3 0)` 503
// (`handle_tr_frames`); `getValues(slot 0 J)` / `(slot 0 D)` answered 34,
// `(slot 2 L)` `L` (null) and `(slot 2 I)` `I 0` (`local_type_mismatch`
// read the variable table only when an entry covered the slot, and an
// unwritten slot was published as a null reference); `modifiers` of both
// arrays answered 0x411 (`debug::jdwp_class_modifiers`). Wave 42 answers
// them as HotSpot does (`commands::handle_tr_frames`,
// `commands::local_read_refusal`, `debug::snapshot_local`,
// `debug::jdwp_class_modifiers`).
//
// Until wave 43 `name(unknown id)` answered 10 (INVALID_THREAD) and
// `signature(unknown id)` 21 (INVALID_CLASS) where HotSpot answers 20
// (INVALID_OBJECT) for an id naming nothing; wave 43 (lane L1) answers 20
// (`debug::unknown_id_refusal`,
// docs/internal/fixed-bugs/interpreter-L1-jdwp-ids-naming-nothing-are-not-invalid-object-FIXED-20261007.md).
// Every CratonVM mode must print HotSpot's transcript.
public class L1W42RawJdwpErrorAnswers {
    /** Set to 1 by the debugger to let `main` call `probe` (`wait` mode). */
    static volatile int go;
    /** Set to 1 by the debugger to let `main` finish (`wait` mode). */
    static volatile int done;
    static long until;
    /** Arrays whose classes the debugger asks about. */
    static String[] strings = {"a"};
    static int[] ints = {1, 2, 3};
    static String probe(int x, String s) {
        String t = s + x;
        return t;
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean wait = args.length == 1 && args[0].equals("wait");
        if (!wait) {
            go = 1;
            done = 1;
        }
        until = System.currentTimeMillis() + 300_000;
        Thread other = new Thread(L1W42RawJdwpErrorAnswers::otherSpin, "other");
        other.setDaemon(true);
        other.start();
        while (go == 0 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        probe(3, "s");
        while (done == 0 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        System.out.println("probed");
    }

    static void otherSpin() {
        try {
            while (System.currentTimeMillis() < until) {
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

        static final int BREAKPOINT = 2;

        static long type;
        static long main;
        static long other;
        static long probeMethod;
        static final java.util.Map<String, Long> arrayFields = new java.util.HashMap<>();

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
            type = awaitClass("LL1W42RawJdwpErrorAnswers;", deadline);
            long goField = 0;
            long doneField = 0;
            r = check(command(2, 4, new Payload().id(type).bytes())); // Fields
            for (int n = r.readInt(); n > 0; n--) {
                long id = r.readLong();
                String name = readString(r);
                readString(r);
                r.readInt();
                if (name.equals("go")) {
                    goField = id;
                } else if (name.equals("done")) {
                    doneField = id;
                } else if (name.equals("strings") || name.equals("ints")) {
                    arrayFields.put(name, id);
                }
            }
            r = check(command(2, 5, new Payload().id(type).bytes())); // Methods
            for (int n = r.readInt(); n > 0; n--) {
                long id = r.readLong();
                String name = readString(r);
                readString(r);
                r.readInt();
                if (name.equals("probe")) {
                    probeMethod = id;
                }
            }
            while (main == 0 || other == 0) {
                r = check(command(1, 4, new byte[0])); // AllThreads
                java.util.List<Long> threads = new java.util.ArrayList<>();
                for (int n = r.readInt(); n > 0; n--) {
                    threads.add(r.readLong());
                }
                for (long t : threads) {
                    Reply named = command(11, 1, new Payload().id(t).bytes());
                    if (named.error != 0) {
                        continue;
                    }
                    String s = readString(named.data());
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

            // The breakpoint at `probe`'s first bytecode, then `go`.
            Payload bp = new Payload();
            bp.u8(BREAKPOINT);
            bp.u8(1); // EVENT_THREAD
            bp.i32(1);
            bp.u8(7); // LocationOnly
            location(bp, probeMethod, 0);
            int bpId = check(command(15, 1, bp.bytes())).readInt();
            setStatic(goField, 1);
            if (awaitEventOf(bpId) == null) {
                throw new IllegalStateException("no breakpoint in probe");
            }
            System.out.println("== main stopped in probe");

            System.out.println("== ThreadReference");
            row("frameCount(running other)", command(11, 7, new Payload().id(other).bytes()), Debugger::int32);
            row("frames(running other)", command(11, 6, frames(other, 0, -1)), Debugger::int32);
            row("ownedMonitors(running other)", command(11, 8, new Payload().id(other).bytes()), Debugger::int32);
            row("frameCount(main)", command(11, 7, new Payload().id(main).bytes()), Debugger::int32);
            row("frames(main 0 -1)", command(11, 6, frames(main, 0, -1)), Debugger::int32);
            row("frames(main 1 1)", command(11, 6, frames(main, 1, 1)), Debugger::int32);
            row("frames(main 2 -1)", command(11, 6, frames(main, 2, -1)), Debugger::int32);
            row("frames(main 2 0)", command(11, 6, frames(main, 2, 0)), Debugger::int32);
            row("frames(main 3 -1)", command(11, 6, frames(main, 3, -1)), Debugger::int32);
            row("frames(main 3 0)", command(11, 6, frames(main, 3, 0)), Debugger::int32);
            row("frames(main 0 3)", command(11, 6, frames(main, 0, 3)), Debugger::int32);
            row("frames(main 1 2)", command(11, 6, frames(main, 1, 2)), Debugger::int32);
            row("frames(main -1 1)", command(11, 6, frames(main, -1, 1)), Debugger::int32);
            row("frames(main 0 -2)", command(11, 6, frames(main, 0, -2)), Debugger::int32);
            row("name(unknown id)", command(11, 1, new Payload().id(0x7fff_fff0L).bytes()), d -> "ok");
            row("status(a class id)", command(11, 4, new Payload().id(type).bytes()), d -> "ok");
            row("suspendCount(other)", command(11, 12, new Payload().id(other).bytes()), Debugger::int32);
            row("resume(other, not suspended)", command(11, 3, new Payload().id(other).bytes()), d -> "ok");
            row("suspendCount(main)", command(11, 12, new Payload().id(main).bytes()), Debugger::int32);
            row("isVirtual(main)", command(11, 15, new Payload().id(main).bytes()),
                    d -> d.readBoolean() ? "true" : "false");

            // `probe(int x, String s)` at bytecode 0: `x` in slot 0, `s` in
            // slot 1, and `t` (slot 2) not yet in scope.
            System.out.println("== StackFrame");
            java.io.DataInputStream f = check(command(11, 6, frames(main, 0, 1)));
            f.readInt();
            long frame = f.readLong();
            row("getValues(slot 0 I)", command(16, 1, getValues(frame, 0, 'I')), Debugger::tagged);
            row("getValues(slot 1 L)", command(16, 1, getValues(frame, 1, 'L')), Debugger::tagged);
            row("getValues(slot 0 J)", command(16, 1, getValues(frame, 0, 'J')), Debugger::tagged);
            row("getValues(slot 0 D)", command(16, 1, getValues(frame, 0, 'D')), Debugger::tagged);
            row("getValues(slot 1 I)", command(16, 1, getValues(frame, 1, 'I')), Debugger::tagged);
            row("getValues(slot 1 J)", command(16, 1, getValues(frame, 1, 'J')), Debugger::tagged);
            row("getValues(slot 2 L)", command(16, 1, getValues(frame, 2, 'L')), Debugger::tagged);
            row("getValues(slot 2 I)", command(16, 1, getValues(frame, 2, 'I')), Debugger::tagged);
            row("getValues(slot 9 I)", command(16, 1, getValues(frame, 9, 'I')), Debugger::tagged);
            row("getValues(unknown frame)", command(16, 1, getValues(0x7fff_0001L, 0, 'I')), Debugger::tagged);
            row("thisObject(static frame)", command(16, 3, new Payload().id(main).id(frame).bytes()),
                    d -> {
                        char tag = (char) d.readUnsignedByte();
                        return tag + (d.readLong() == 0 ? " null" : " object");
                    });

            System.out.println("== ReferenceType");
            row("signature(unknown id)", command(2, 1, new Payload().id(0x7fff_fff0L).bytes()), d -> "ok");
            row("signature(a thread id)", command(2, 1, new Payload().id(main).bytes()), d -> "ok");
            row("status(probe class)", command(2, 9, new Payload().id(type).bytes()), Debugger::int32);
            row("classFileVersion(probe class)", command(2, 17, new Payload().id(type).bytes()),
                    d -> d.readInt() + "." + d.readInt());
            // Each array class as the debugger meets it: the type of an array
            // a static field holds (ReferenceType.GetValues, then
            // ObjectReference.ReferenceType).
            for (String field : new String[] {"strings", "ints"}) {
                String array = field.equals("strings") ? "String[]" : "int[]";
                Payload read = new Payload().id(type);
                read.i32(1);
                read.id(arrayFields.get(field));
                java.io.DataInputStream v = check(command(2, 6, read.bytes())); // GetValues
                v.readInt();
                v.readUnsignedByte();
                java.io.DataInputStream t = check(command(9, 1, new Payload().id(v.readLong()).bytes()));
                int tag = t.readUnsignedByte();
                long a = t.readLong();
                System.out.println("  referenceType(" + array + "): tag " + tag);
                row("sourceFile(" + array + ")", command(2, 7, new Payload().id(a).bytes()), Debugger::string);
                row("status(" + array + ")", command(2, 9, new Payload().id(a).bytes()), Debugger::int32);
                row("modifiers(" + array + ")", command(2, 3, new Payload().id(a).bytes()),
                        d -> "0x" + Integer.toHexString(d.readInt()));
                row("methods(" + array + ")", command(2, 15, new Payload().id(a).bytes()), Debugger::int32);
                row("fields(" + array + ")", command(2, 14, new Payload().id(a).bytes()), Debugger::int32);
                row("interfaces(" + array + ")", command(2, 10, new Payload().id(a).bytes()), Debugger::int32);
                row("classFileVersion(" + array + ")", command(2, 17, new Payload().id(a).bytes()),
                        d -> d.readInt() + "." + d.readInt());
            }

            // The breakpoint in `probe` is the only request; let `main` go.
            check(command(15, 3, new byte[0])); // ClearAllBreakpoints
            check(command(11, 3, new Payload().id(main).bytes())); // Resume
            setStatic(doneField, 1);
            try {
                command(1, 6, new byte[0]); // Dispose
            } catch (java.io.IOException exited) {
                // The debuggee may exit and close the socket first.
            }
            System.out.println("== done");
            socket.close();
        }

        interface Gist {
            String of(java.io.DataInputStream d) throws Exception;
        }

        static void row(String what, Reply reply, Gist gist) throws Exception {
            String answer = reply.error != 0 ? "error " + reply.error : gist.of(reply.data());
            System.out.println("  " + what + ": " + answer);
        }

        static String int32(java.io.DataInputStream d) throws Exception {
            return Integer.toString(d.readInt());
        }

        static String string(java.io.DataInputStream d) throws Exception {
            return readString(d);
        }

        /** A `GetValues` reply's one value: its tag, and an int's value. */
        static String tagged(java.io.DataInputStream d) throws Exception {
            d.readInt();
            char tag = (char) d.readUnsignedByte();
            return tag == 'I' ? "I " + d.readInt() : String.valueOf(tag);
        }

        static byte[] frames(long thread, int start, int length) throws Exception {
            Payload p = new Payload().id(thread);
            p.i32(start);
            p.i32(length);
            return p.bytes();
        }

        static byte[] getValues(long frame, int slot, char sig) throws Exception {
            Payload p = new Payload().id(main).id(frame);
            p.i32(1);
            p.i32(slot);
            p.u8(sig);
            return p.bytes();
        }

        static void location(Payload p, long method, long index) throws Exception {
            p.u8(1); // CLASS
            p.id(type);
            p.id(method);
            p.id(index);
        }

        static void setStatic(long field, int value) throws Exception {
            Payload p = new Payload().id(type);
            p.i32(1);
            p.id(field);
            p.i32(value);
            check(command(3, 2, p.bytes())); // ClassType.SetValues
        }

        static long awaitClass(String signature, long deadline) throws Exception {
            while (true) {
                Payload p = new Payload();
                p.string(signature);
                java.io.DataInputStream r = check(command(1, 2, p.bytes())); // ClassesBySignature
                if (r.readInt() > 0) {
                    r.readByte();
                    return r.readLong();
                }
                if (System.currentTimeMillis() > deadline) {
                    throw new IllegalStateException(signature + " never loaded");
                }
                Thread.sleep(50);
            }
        }

        /** The data of the next event set holding an event of `requestId`, or null after 60 s. */
        static byte[] awaitEventOf(int requestId) throws Exception {
            long until = System.currentTimeMillis() + 60_000;
            while (System.currentTimeMillis() < until) {
                while (events.isEmpty()) {
                    readPacket(-1);
                }
                byte[] set = events.poll();
                java.io.DataInputStream d = new java.io.DataInputStream(
                        new java.io.ByteArrayInputStream(set));
                d.readUnsignedByte(); // suspendPolicy
                if (d.readInt() > 0) {
                    d.readUnsignedByte(); // eventKind
                    if (d.readInt() == requestId) {
                        return set;
                    }
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
