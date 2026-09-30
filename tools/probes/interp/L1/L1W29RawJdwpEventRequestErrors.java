// Interpreter round i1 wave 29, lane L1: a raw JDWP scenario for the
// validation of `EventRequest.Set` / `Clear` (review of
// `vm/src/debug/commands.rs` `handle_er_set` / `handle_er_clear`).
//
// JDI validates most requests before it sends them, so a JDI debugger never
// sees the back end's answer to a malformed one; any other JDWP client
// (an IDE's own JDWP layer, a test harness) does. This debugger speaks JDWP
// over a raw socket and prints the error code of each command, one row each:
// modifiers a request kind does not allow, a Count of 0 or less, missing
// required modifiers, invalid step sizes and depths, ids that name nothing,
// and clears of requests that do not exist. Every request that succeeds is
// cleared again, so none of them fires.
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
//   javac -g -d out L1W29RawJdwpEventRequestErrors.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L1W29RawJdwpEventRequestErrors wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5791 -cp out L1W29RawJdwpEventRequestErrors wait)
//   java -cp out L1W29RawJdwpEventRequestErrors debug 5791 > transcript.txt
//
// HotSpot 25.0.3 as the debuggee (the debugger's stdout):
//
//   == attach
//   == required modifiers
//     breakpoint, no location: error 113
//     single step, no step: ok
//     field access, no field: error 113
//     method entry, none: ok
//   == count
//     breakpoint, count 0: error 113
//     breakpoint, count -1: error 113
//     breakpoint, count 1: ok
//   == modifiers a kind does not allow
//     thread start, location: error 103
//     thread start, class only: error 103
//     thread start, class match: error 103
//     class prepare, exception only: error 103
//     method entry, field only: error 103
//     method entry, step: error 103
//     method entry, location: error 103
//     exception, location: ok
//     breakpoint, two locations: ok
//   == bad values
//     step, size 5: ok
//     step, depth 7: ok
//     step, no such thread: error 20
//     method entry, no such thread: error 20
//     method entry, no such class: error 20
//     method entry, policy 7: ok
//     breakpoint, index past the end: error 24
//     event kind 77: error 102
//   == duplicate step
//     first step: ok
//     second step, same thread: ok
//   == clear
//     clear no such request: ok
//     clear request 0: ok
//     clear event kind 77: error 102
//     clear with another kind: ok
//     clear with its kind: ok
//     clear it again: ok
//   == done
//
// CratonVM before wave 29 (from reading `commands::handle_er_set` /
// `handle_er_clear`): `ok` on every row of "required modifiers", "count" and
// "modifiers a kind does not allow" (except the rows that print `ok` above),
// on "breakpoint, index past the end" and on "clear event kind 77".
// Wave 29 (`commands::event_request_refusal`) answers them as HotSpot does.
// The three "no such thread / class" rows answer 20 as HotSpot does since the
// wave-29 orchestrator's fix, and "method entry, policy 7" is accepted since
// wave 37 (it answered 113;
// docs/internal/fixed-bugs/interpreter-L1-jdwp-event-request-filters-naming-no-thread-or-class-are-accepted-FIXED-20261001.md,
// and `L1W37RawJdwpUnknownSuspendPolicy` for what such a request suspends).
public class L1W29RawJdwpEventRequestErrors {
    /** Set to 1 by the debugger to let `main` finish (`wait` mode). */
    static volatile int done;
    static int watched;
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
        spin();
        System.out.println("spun");
    }

    static void spin() throws InterruptedException {
        while (done == 0 && System.currentTimeMillis() < until) {
            watched++;
            Thread.sleep(20);
        }
    }

    /** A minimal JDWP client (8-byte ids, as both VMs answer `IDSizes`). */
    static final class Debugger {
        static java.io.DataInputStream in;
        static java.io.DataOutputStream out;
        static int nextId = 1;

        // Event kinds.
        static final int SINGLE_STEP = 1;
        static final int BREAKPOINT = 2;
        static final int EXCEPTION = 4;
        static final int THREAD_START = 6;
        static final int CLASS_PREPARE = 8;
        static final int FIELD_ACCESS = 20;
        static final int METHOD_ENTRY = 40;

        static long type;
        static long spin;
        static long watchedField;
        static long main;

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
                p.string("LL1W29RawJdwpEventRequestErrors;");
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
                } else if (name.equals("watched")) {
                    watchedField = id;
                }
            }
            r = check(command(2, 5, new Payload().id(type).bytes())); // Methods
            for (int n = r.readInt(); n > 0; n--) {
                long id = r.readLong();
                String name = readString(r);
                readString(r);
                r.readInt();
                if (name.equals("spin")) {
                    spin = id;
                }
            }
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
            if (main == 0 || spin == 0 || watchedField == 0) {
                throw new IllegalStateException("setup incomplete");
            }

            System.out.println("== required modifiers");
            row("breakpoint, no location", set(BREAKPOINT, 0));
            row("single step, no step", set(SINGLE_STEP, 0));
            row("field access, no field", set(FIELD_ACCESS, 0));
            row("method entry, none", set(METHOD_ENTRY, 0));

            System.out.println("== count");
            row("breakpoint, count 0", set(BREAKPOINT, 0, count(0), location(0)));
            row("breakpoint, count -1", set(BREAKPOINT, 0, count(-1), location(0)));
            row("breakpoint, count 1", set(BREAKPOINT, 0, count(1), location(0)));

            System.out.println("== modifiers a kind does not allow");
            row("thread start, location", set(THREAD_START, 0, location(0)));
            row("thread start, class only", set(THREAD_START, 0, classOnly(type)));
            row("thread start, class match", set(THREAD_START, 0, classMatch("x.*")));
            row("class prepare, exception only", set(CLASS_PREPARE, 0, exceptionOnly()));
            row("method entry, field only", set(METHOD_ENTRY, 0, fieldOnly()));
            row("method entry, step", set(METHOD_ENTRY, 0, step(main, 1, 0)));
            row("method entry, location", set(METHOD_ENTRY, 0, location(0)));
            row("exception, location", set(EXCEPTION, 0, location(0)));
            row("breakpoint, two locations", set(BREAKPOINT, 0, location(0), location(0)));

            System.out.println("== bad values");
            row("step, size 5", set(SINGLE_STEP, 0, step(main, 5, 0)));
            row("step, depth 7", set(SINGLE_STEP, 0, step(main, 1, 7)));
            // No row for a Step or ThreadOnly naming thread 0 or a ClassOnly
            // naming class 0 (null): HotSpot 25.0.3's back end dies on each
            // (FATAL ERROR "JDWP saveGlobalRef obj", AGENT_ERROR_ILLEGAL_ARGUMENT).
            row("step, no such thread", set(SINGLE_STEP, 0, step(0x7654_3210L, 1, 0)));
            row("method entry, no such thread", set(METHOD_ENTRY, 0, threadOnly(0x7654_3210L)));
            row("method entry, no such class", set(METHOD_ENTRY, 0, classOnly(0x7654_3210L)));
            row("method entry, policy 7", set(METHOD_ENTRY, 7));
            row("breakpoint, index past the end", set(BREAKPOINT, 0, location(100_000)));
            row("event kind 77", set(77, 0));

            System.out.println("== duplicate step");
            Reply first = set(SINGLE_STEP, 0, step(main, 1, 0));
            System.out.println("  first step: " + (first.error == 0 ? "ok" : "error " + first.error));
            row("second step, same thread", set(SINGLE_STEP, 0, step(main, 1, 0)));
            first.cleanup.run();

            System.out.println("== clear");
            row("clear no such request", clear(BREAKPOINT, 0x7654_3210));
            row("clear request 0", clear(BREAKPOINT, 0));
            row("clear event kind 77", clear(77, 1));
            Reply entry = set(METHOD_ENTRY, 0, classOnly(type));
            int entryId = entry.error == 0 ? entry.data().readInt() : 0;
            row("clear with another kind", clear(BREAKPOINT, entryId));
            row("clear with its kind", clear(METHOD_ENTRY, entryId));
            row("clear it again", clear(METHOD_ENTRY, entryId));

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

        /** Print a row; clear the request when it was accepted. */
        static void row(String what, Reply reply) throws Exception {
            if (reply.error != 0) {
                System.out.println("  " + what + ": error " + reply.error);
                return;
            }
            System.out.println("  " + what + ": ok");
            reply.cleanup.run();
        }

        interface Cleanup {
            void run() throws Exception;
        }

        static Reply set(int kind, int policy, byte[]... modifiers) throws Exception {
            Payload p = new Payload();
            p.u8(kind);
            p.u8(policy);
            p.i32(modifiers.length);
            for (byte[] m : modifiers) {
                p.raw(m);
            }
            Reply reply = command(15, 1, p.bytes());
            if (reply.error == 0) {
                int id = reply.data().readInt();
                reply.cleanup = () -> clear(kind, id);
            }
            return reply;
        }

        static Reply clear(int kind, int id) throws Exception {
            Payload p = new Payload();
            p.u8(kind);
            p.i32(id);
            return command(15, 2, p.bytes());
        }

        static byte[] count(int n) throws Exception {
            Payload p = new Payload();
            p.u8(1);
            p.i32(n);
            return p.bytes();
        }

        static byte[] threadOnly(long thread) throws Exception {
            Payload p = new Payload();
            p.u8(3);
            p.id(thread);
            return p.bytes();
        }

        static byte[] classOnly(long clazz) throws Exception {
            Payload p = new Payload();
            p.u8(4);
            p.id(clazz);
            return p.bytes();
        }

        static byte[] classMatch(String pattern) throws Exception {
            Payload p = new Payload();
            p.u8(5);
            p.string(pattern);
            return p.bytes();
        }

        static byte[] location(long index) throws Exception {
            Payload p = new Payload();
            p.u8(7);
            p.u8(1); // TypeTag.CLASS
            p.id(type);
            p.id(spin);
            p.id(index);
            return p.bytes();
        }

        static byte[] exceptionOnly() throws Exception {
            Payload p = new Payload();
            p.u8(8);
            p.id(0);
            p.u8(1);
            p.u8(1);
            return p.bytes();
        }

        static byte[] fieldOnly() throws Exception {
            Payload p = new Payload();
            p.u8(9);
            p.id(type);
            p.id(watchedField);
            return p.bytes();
        }

        static byte[] step(long thread, int size, int depth) throws Exception {
            Payload p = new Payload();
            p.u8(10);
            p.id(thread);
            p.i32(size);
            p.i32(depth);
            return p.bytes();
        }

        static final class Reply {
            int error;
            byte[] body;
            Cleanup cleanup = () -> { };

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
