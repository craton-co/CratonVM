// Interpreter round i1 wave 28, lane L1: a raw JDWP scenario for
// docs/internal/fixed-bugs/interpreter-L1-jdwp-frame-ids-are-reused-across-suspensions-FIXED-20260929.md
// (in tools/jdi/run-jdi-conformance.sh's default list; `--scenario
// L1W28RawJdwpStaleFrameId` runs it alone).
//
// A JDWP frame id is valid only for the suspension it was listed in: HotSpot
// folds the thread's frame generation, bumped on every resume, into each id
// (`threadControl.c` `createFrameID`) and refuses an older one with
// INVALID_FRAMEID (30). JDI never sends a stale id (it invalidates its
// `StackFrame` mirrors on resume), so this debugger speaks JDWP over a raw
// socket:
//
//  * suspend the VM while `main` is in `a(3)`, list the main thread's frames
//    and keep the id of `a`'s frame;
//  * set `phase`, resume, and suspend again once `main` is in `b(7)` (called
//    from the same method, at the same depth);
//  * ask `StackFrame.GetValues` for slot 0 (`I`) of the OLD id, and of `b`'s
//    new id.
//
// Canonical transcript: no ids, no addresses, no timings.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   a+b=10
//
// Under a debugger (the debuggee waits up to five minutes in `a` for the
// debugger to set the static `phase`):
//
//   javac -g -d out L1W28RawJdwpStaleFrameId.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L1W28RawJdwpStaleFrameId wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5791 -cp out L1W28RawJdwpStaleFrameId wait)
//   java -cp out L1W28RawJdwpStaleFrameId debug 5791 > transcript.txt
//
// HotSpot 25.0.3 as the debuggee (the debugger's stdout):
//
//   == attach
//   == suspended in a: a.y=value 3
//   == suspended in b: b.x=value 7
//   old id listed again: false
//   old id GetValues: error 30
//   == done
//
// CratonVM before the fix (frame ids were bare positions from the top) lists
// the old id again and answers it with `b`'s frame, which stands at `a`'s
// old position: `old id listed again: true` and `old id GetValues: value 7`
// (derived from the code, not run: the JDWP server needs an
// `experimental-debug` build).
public class L1W28RawJdwpStaleFrameId {
    /** Set to 1 by the debugger to let `a` return (`wait` mode). */
    static volatile int phase;
    /** Set to 1 by the debugger to let `b` return (`wait` mode). */
    static volatile int done;
    static long until;

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean wait = args.length == 1 && args[0].equals("wait");
        if (!wait) {
            phase = 1;
            done = 1;
        }
        until = System.currentTimeMillis() + 300_000;
        int r = a(3);
        r += b(7);
        System.out.println("a+b=" + r);
    }

    static int a(int y) throws InterruptedException {
        while (phase == 0 && System.currentTimeMillis() < until) {
            Thread.sleep(20);
        }
        return y;
    }

    static int b(int x) throws InterruptedException {
        while (done == 0 && System.currentTimeMillis() < until) {
            Thread.sleep(20);
        }
        return x;
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

            // The probe class, its fields and methods.
            long type = 0;
            while (type == 0) {
                Payload p = new Payload();
                p.string("LL1W28RawJdwpStaleFrameId;");
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
            java.util.Map<Long, String> methods = new java.util.HashMap<>();
            r = check(command(2, 5, new Payload().id(type).bytes())); // Methods
            for (int n = r.readInt(); n > 0; n--) {
                long id = r.readLong();
                String name = readString(r);
                readString(r);
                r.readInt();
                methods.put(id, name);
            }

            // The main thread.
            long main = 0;
            r = check(command(1, 4, new byte[0])); // AllThreads
            java.util.List<Long> threads = new java.util.ArrayList<>();
            for (int n = r.readInt(); n > 0; n--) {
                threads.add(r.readLong());
            }
            for (long t : threads) {
                java.io.DataInputStream name = check(command(11, 1, new Payload().id(t).bytes()));
                if (readString(name).equals("main")) {
                    main = t;
                }
            }
            if (main == 0) {
                throw new IllegalStateException("no main thread");
            }

            long[] inA = suspendIn(main, "a", methods, deadline);
            System.out.println("== suspended in a: a.y=" + describe(getInt(main, inA[0])));
            long oldId = inA[0];

            // Let `a` return; suspend again once `b` runs.
            setStatic(type, fields.get("phase"), 1);
            check(command(1, 9, new byte[0])); // VirtualMachine.Resume
            long[] inB = suspendIn(main, "b", methods, deadline);
            System.out.println("== suspended in b: b.x=" + describe(getInt(main, inB[0])));
            boolean listed = false;
            for (long[] frame : frames(main)) {
                listed |= frame[0] == oldId;
            }
            System.out.println("old id listed again: " + listed);
            System.out.println("old id GetValues: " + describe(getInt(main, oldId)));

            setStatic(type, fields.get("done"), 1);
            check(command(1, 9, new byte[0])); // VirtualMachine.Resume
            try {
                command(1, 6, new byte[0]); // Dispose
            } catch (java.io.IOException exited) {
                // The resumed debuggee may exit and close the socket first
                // (HotSpot 25 does, now and then): the run is over either way.
            }
            System.out.println("== done");
            socket.close();
        }

        /**
         * Suspend the VM until the main thread has a frame of the probe's
         * method `name`: answers {frame id}, the VM left suspended.
         */
        static long[] suspendIn(long thread, String name, java.util.Map<Long, String> methods,
                long deadline) throws Exception {
            while (true) {
                check(command(1, 8, new byte[0])); // VirtualMachine.Suspend
                for (long[] frame : frames(thread)) {
                    if (name.equals(methods.get(frame[1]))) {
                        return new long[] {frame[0]};
                    }
                }
                check(command(1, 9, new byte[0]));
                if (System.currentTimeMillis() > deadline) {
                    throw new IllegalStateException("never in " + name);
                }
                Thread.sleep(50);
            }
        }

        /** `ThreadReference.Frames`: {frame id, method id} per frame, top first. */
        static java.util.List<long[]> frames(long thread) throws Exception {
            Payload p = new Payload().id(thread);
            p.i32(0);
            p.i32(-1);
            java.io.DataInputStream r = check(command(11, 6, p.bytes()));
            java.util.List<long[]> list = new java.util.ArrayList<>();
            for (int n = r.readInt(); n > 0; n--) {
                long frame = r.readLong();
                r.readByte();
                r.readLong();
                long method = r.readLong();
                r.readLong();
                list.add(new long[] {frame, method});
            }
            return list;
        }

        /** `StackFrame.GetValues` of slot 0 as an int: {error, value}. */
        static int[] getInt(long thread, long frame) throws Exception {
            Payload p = new Payload().id(thread).id(frame);
            p.i32(1);
            p.i32(0);
            p.u8('I');
            Reply reply = command(16, 1, p.bytes());
            if (reply.error != 0) {
                return new int[] {reply.error, 0};
            }
            java.io.DataInputStream r = reply.data();
            r.readInt();
            r.readByte();
            return new int[] {0, r.readInt()};
        }

        static String describe(int[] answer) {
            return answer[0] != 0 ? "error " + answer[0] : "value " + answer[1];
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
