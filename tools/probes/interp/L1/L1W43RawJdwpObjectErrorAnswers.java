// Interpreter round i1 wave 43, lane L1: the answers, and the error codes, of
// the JDWP ClassType, ArrayType, ArrayReference, ObjectReference,
// StringReference, ClassObjectReference, ClassLoaderReference and
// ModuleReference commands at their edges, and of ForceEarlyReturn, from a
// raw JDWP client (the style of L1W42RawJdwpErrorAnswers; JDI checks most of
// these arguments itself, a raw client does not).
//
// The debuggee's `main` waits for the debugger to set the static `go`, then
// calls `probe()`, where a breakpoint (suspend policy EVENT_THREAD) stops it
// at bytecode index 0. A daemon thread `other` sleeps in a loop, never
// suspended. The objects asked about are the statics of the probe class,
// read with ReferenceType.GetValues. One line per command,
// `<command>(<case>): <answer>`, where the answer is `error <code>` or the
// reply's gist.
//
// Canonical transcript: no ids, no addresses, no timings.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   probed 1
//
// Under a debugger (the debuggee waits up to five minutes for `go`):
//
//   javac -g -d out L1W43RawJdwpObjectErrorAnswers.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L1W43RawJdwpObjectErrorAnswers wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5791 -cp out L1W43RawJdwpObjectErrorAnswers wait)
//   java -cp out L1W43RawJdwpObjectErrorAnswers debug 5791 > transcript.txt
//
// HotSpot 25.0.3 as the debuggee (the debugger's stdout):
//
//   == attach
//   == main stopped in probe
//   == ClassType
//     superclass(probe class): an id
//     superclass(Object): null
//     superclass(an interface): null
//     superclass(unknown id): error 20
//     superclass(an object id): error 21
//     setValues(a static int): ok
//     invokeMethod(add 2 3): I 5
//     invokeMethod(on running other): error 10
//     invokeMethod(unknown thread): error 20
//     newInstance(Box): L an id
//     newInstance(an abstract class): L null threw
//   == ArrayType
//     newInstance(int[] 4): [ an id
//     newInstance(String[] 0): [ an id
//     newInstance(int[] -1): error 110
//     newInstance(unknown id): error 20
//   == ArrayReference
//     length(int[]): 3
//     length(a string): error 508
//     length(unknown id): error 20
//     getValues(int[] 0 3): I [1 2 3]
//     getValues(int[] 1 2): I [2 3]
//     getValues(int[] 3 0): error 503
//     getValues(int[] 0 4): error 504
//     getValues(int[] 4 0): error 503
//     getValues(int[] -1 1): error 503
//     getValues(int[] 0 -1): I [1 2 3]
//     getValues(String[] 0 2): L [s id s id]
//     getValues(a string): error 508
//     setValues(int[] 1 [9]): ok
//     setValues(int[] 3 [9]): error 503
//     setValues(int[] 2 [9 9]): error 504
//     setValues(int[] -1 [9]): error 503
//     getValues(int[] after the writes): I [1 9 3]
//   == ObjectReference
//     referenceType(unknown id): error 20
//     getValues(box x): I 5
//     getValues(unknown id): error 20
//     setValues(box x 7): ok
//     getValues(box x after): I 7
//     invokeMethod(box twice 4): I 8
//     invokeMethod(box, a static method): I 9
//     isCollected(box): false
//     isCollected(unknown id): true
//     disableCollection(unknown id): error 20
//     enableCollection(unknown id): ok
//   == StringReference
//     value(str): "hello"
//     value(an int[]): error 506
//     value(unknown id): error 20
//   == ClassObjectReference
//     reflectedType(Integer.class): tag 1
//     reflectedType(unknown id): error 20
//   == ClassLoaderReference
//     visibleClasses(app loader) has Box: yes
//     visibleClasses(a string): error 507
//     visibleClasses(unknown id): error 20
//   == ModuleReference
//     name(java.base): "java.base"
//     name(unnamed): ""
//     name(unknown id): error 42
//     classLoader(java.base): null
//     classLoader(unnamed): an id
//     module(probe class) is unnamed: yes
//   == ForceEarlyReturn
//     forceEarlyReturn(running other): error 32
//     forceEarlyReturn(unknown thread): error 20
//     forceEarlyReturn(main, J): error 34
//     forceEarlyReturn(main, V): error 34
//     forceEarlyReturn(main, L): error 34
//     forceEarlyReturn(main, Z): ok
//     forceEarlyReturn(main, I 1): error 113
//   == done
//
// (Measured on the Windows box, twice alike; the orchestrator should take the
// host's HotSpot run as the reference.) Left out because HotSpot crashes the
// debuggee or answers from undefined behaviour on them: an unknown field or
// method id (they are raw pointers there), a field or method of another
// class, `ArrayReference.Length(null)`, `ClassType.InvokeMethod` with too few
// arguments, `ClassObjectReference.ReflectedType` and
// `ModuleReference.ClassLoader` of an object of another kind (a JDWP agent
// fatal error / a crash); `ModuleReference.Name` of a string answers "" and
// `ArrayType.NewInstance` of a class type answers `NOT_FOUND` (41) or another
// code depending on the class's name, where CratonVM answers
// `INVALID_MODULE` / `INVALID_CLASS`.
//
// CratonVM before wave 43, from reading `debug::commands`, `debug::inspect`
// and `debug::SharedVmBridge`:
//
//   superclass(an object id): null            (no class check)
//   invokeMethod(on running other): error 13  (`THREAD_NOT_SUSPENDED`)
//   invokeMethod(unknown thread): error 10
//   newInstance(int[] -1): error 103          (`ILLEGAL_ARGUMENT`)
//   getValues(int[] 3 0): I []
//   getValues(int[] 0 -1): error 504
//   setValues(int[] 3 [9]): error 504
//   invokeMethod(box, a static method): not I 9 (the receiver was passed
//     as the method's first argument)
//   isCollected(unknown id): error 20
//   enableCollection(unknown id): error 20
//   visibleClasses(a string): no            (the bootstrap loader's classes)
//   name(unknown id): error 20
//   forceEarlyReturn(...): error 99 on every row (not served)
//
// (and `superclass(an interface)` answered an id: the interface's class-file
// `java/lang/Object`). Wave 43 answers each as HotSpot does
// (`commands::handle_ct_superclass`, `SharedVmBridge::not_parked`,
// `SharedVmBridge::invoke`, `commands::prepare_at_new_instance`,
// `inspect::region`, `inspect::or_is_collected`, `inspect::set_collection`,
// `inspect::visible_classes`, `inspect::module_command`,
// `debug::early_return`). Every CratonVM mode must print HotSpot's
// transcript.
public class L1W43RawJdwpObjectErrorAnswers {
    /** Set to 1 by the debugger to let `main` call `probe` (`wait` mode). */
    static volatile int go;
    /** Set to 1 by the debugger to let `main` finish (`wait` mode). */
    static volatile int done;
    static long until;

    static String str = "hello";
    static int[] ints = {1, 2, 3};
    static String[] strs = {"a", "b"};
    static Object obj = new Object();
    static ClassLoader loader = L1W43RawJdwpObjectErrorAnswers.class.getClassLoader();
    static Module base = Object.class.getModule();
    static Module unnamed = L1W43RawJdwpObjectErrorAnswers.class.getModule();
    static Class<?> cls = Integer.class;
    static Box box = new Box();

    static final class Box {
        int x = 5;
        static int s = 6;

        Box() {
        }

        int twice(int a) {
            return 2 * a;
        }

        static int add(int a, int b) {
            return a + b;
        }
    }

    abstract static class Shape {
        Shape() {
        }
    }

    interface Named {
        String name();
    }

    static int probe() {
        return 1;
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
        Thread other = new Thread(L1W43RawJdwpObjectErrorAnswers::otherSpin, "other");
        other.setDaemon(true);
        other.start();
        // Loaded, so the debugger finds them by signature.
        Class.forName(Shape.class.getName());
        Class.forName(Named.class.getName());
        while (go == 0 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        int p = probe();
        while (done == 0 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        System.out.println("probed " + p);
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
        static final long UNKNOWN = 0x7fff_fff0L;

        static long type;
        static long boxType;
        static long shapeType;
        static long namedType;
        static long objectType;
        static long main;
        static long other;
        static long probeMethod;
        static final java.util.Map<String, Long> statics = new java.util.HashMap<>();
        static final java.util.Map<String, Long> boxFields = new java.util.HashMap<>();
        static final java.util.Map<String, Long> boxMethods = new java.util.HashMap<>();
        static long shapeInit;

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
            type = awaitClass("LL1W43RawJdwpObjectErrorAnswers;", deadline);
            boxType = awaitClass("LL1W43RawJdwpObjectErrorAnswers$Box;", deadline);
            shapeType = awaitClass("LL1W43RawJdwpObjectErrorAnswers$Shape;", deadline);
            namedType = awaitClass("LL1W43RawJdwpObjectErrorAnswers$Named;", deadline);
            objectType = awaitClass("Ljava/lang/Object;", deadline);
            long goField = 0;
            long doneField = 0;
            r = check(command(2, 4, new Payload().id(type).bytes())); // Fields
            java.util.Map<Long, String> staticNames = new java.util.LinkedHashMap<>();
            for (int n = r.readInt(); n > 0; n--) {
                long id = r.readLong();
                String name = readString(r);
                readString(r);
                r.readInt();
                if (name.equals("go")) {
                    goField = id;
                } else if (name.equals("done")) {
                    doneField = id;
                } else if (!name.equals("until")) {
                    staticNames.put(id, name);
                }
            }
            for (java.util.Map.Entry<Long, String> e : staticNames.entrySet()) {
                Payload read = new Payload().id(type);
                read.i32(1);
                read.id(e.getKey());
                java.io.DataInputStream v = check(command(2, 6, read.bytes())); // GetValues
                v.readInt();
                v.readUnsignedByte();
                statics.put(e.getValue(), v.readLong());
            }
            r = check(command(2, 4, new Payload().id(boxType).bytes()));
            for (int n = r.readInt(); n > 0; n--) {
                long id = r.readLong();
                String name = readString(r);
                readString(r);
                r.readInt();
                boxFields.put(name, id);
            }
            r = check(command(2, 5, new Payload().id(boxType).bytes()));
            for (int n = r.readInt(); n > 0; n--) {
                long id = r.readLong();
                String name = readString(r);
                readString(r);
                r.readInt();
                boxMethods.put(name, id);
            }
            r = check(command(2, 5, new Payload().id(shapeType).bytes()));
            for (int n = r.readInt(); n > 0; n--) {
                long id = r.readLong();
                String name = readString(r);
                readString(r);
                r.readInt();
                if (name.equals("<init>")) {
                    shapeInit = id;
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
            bp.u8(1); // CLASS
            bp.id(type);
            bp.id(probeMethod);
            bp.id(0);
            int bpId = check(command(15, 1, bp.bytes())).readInt();
            setStatic(goField, 1);
            if (awaitEventOf(bpId) == null) {
                throw new IllegalStateException("no breakpoint in probe");
            }
            System.out.println("== main stopped in probe");

            long str = statics.get("str");
            long ints = statics.get("ints");
            long strs = statics.get("strs");
            long obj = statics.get("obj");
            long loader = statics.get("loader");
            long base = statics.get("base");
            long unnamed = statics.get("unnamed");
            long cls = statics.get("cls");
            long box = statics.get("box");
            long boxX = boxFields.get("x");
            long boxS = boxFields.get("s");

            System.out.println("== ClassType");
            row("superclass(probe class)", command(3, 1, new Payload().id(type).bytes()), Debugger::nullOrNot);
            row("superclass(Object)", command(3, 1, new Payload().id(objectType).bytes()), Debugger::nullOrNot);
            row("superclass(an interface)", command(3, 1, new Payload().id(namedType).bytes()), Debugger::nullOrNot);
            row("superclass(unknown id)", command(3, 1, new Payload().id(UNKNOWN).bytes()), Debugger::nullOrNot);
            row("superclass(an object id)", command(3, 1, new Payload().id(obj).bytes()), Debugger::nullOrNot);
            row("setValues(a static int)", command(3, 2, setInt(boxType, boxS, 16)), d -> "ok");
            row("invokeMethod(add 2 3)", command(3, 3, invoke(boxType, main, boxMethods.get("add"), 2, 3)),
                    Debugger::invokeResult);
            row("invokeMethod(on running other)", command(3, 3,
                    invoke(boxType, other, boxMethods.get("add"), 2, 3)), Debugger::invokeResult);
            row("invokeMethod(unknown thread)", command(3, 3,
                    invoke(boxType, UNKNOWN, boxMethods.get("add"), 2, 3)), Debugger::invokeResult);
            row("newInstance(Box)", command(3, 4, invoke(boxType, main, boxMethods.get("<init>"))),
                    Debugger::invokeResult);
            row("newInstance(an abstract class)", command(3, 4, invoke(shapeType, main, shapeInit)),
                    Debugger::invokeResult);

            System.out.println("== ArrayType");
            long intArrayType = referenceType(ints);
            long stringArrayType = referenceType(strs);
            row("newInstance(int[] 4)", command(4, 1, newArray(intArrayType, 4)), Debugger::taggedObject);
            row("newInstance(String[] 0)", command(4, 1, newArray(stringArrayType, 0)), Debugger::taggedObject);
            row("newInstance(int[] -1)", command(4, 1, newArray(intArrayType, -1)), Debugger::taggedObject);
            row("newInstance(unknown id)", command(4, 1, newArray(UNKNOWN, 4)), Debugger::taggedObject);

            System.out.println("== ArrayReference");
            row("length(int[])", command(13, 1, new Payload().id(ints).bytes()), Debugger::int32);
            row("length(a string)", command(13, 1, new Payload().id(str).bytes()), Debugger::int32);
            row("length(unknown id)", command(13, 1, new Payload().id(UNKNOWN).bytes()), Debugger::int32);
            row("getValues(int[] 0 3)", command(13, 2, range(ints, 0, 3)), Debugger::arrayValues);
            row("getValues(int[] 1 2)", command(13, 2, range(ints, 1, 2)), Debugger::arrayValues);
            row("getValues(int[] 3 0)", command(13, 2, range(ints, 3, 0)), Debugger::arrayValues);
            row("getValues(int[] 0 4)", command(13, 2, range(ints, 0, 4)), Debugger::arrayValues);
            row("getValues(int[] 4 0)", command(13, 2, range(ints, 4, 0)), Debugger::arrayValues);
            row("getValues(int[] -1 1)", command(13, 2, range(ints, -1, 1)), Debugger::arrayValues);
            row("getValues(int[] 0 -1)", command(13, 2, range(ints, 0, -1)), Debugger::arrayValues);
            row("getValues(String[] 0 2)", command(13, 2, range(strs, 0, 2)), Debugger::arrayValues);
            row("getValues(a string)", command(13, 2, range(str, 0, 1)), Debugger::arrayValues);
            row("setValues(int[] 1 [9])", command(13, 3, setInts(ints, 1, 9)), d -> "ok");
            row("setValues(int[] 3 [9])", command(13, 3, setInts(ints, 3, 9)), d -> "ok");
            row("setValues(int[] 2 [9 9])", command(13, 3, setInts(ints, 2, 9, 9)), d -> "ok");
            row("setValues(int[] -1 [9])", command(13, 3, setInts(ints, -1, 9)), d -> "ok");
            row("getValues(int[] after the writes)", command(13, 2, range(ints, 0, 3)), Debugger::arrayValues);

            System.out.println("== ObjectReference");
            row("referenceType(unknown id)", command(9, 1, new Payload().id(UNKNOWN).bytes()), d -> "ok");
            row("getValues(box x)", command(9, 2, fields(box, boxX)), Debugger::values);
            row("getValues(unknown id)", command(9, 2, fields(UNKNOWN, boxX)), Debugger::values);
            row("setValues(box x 7)", command(9, 3, setField(box, boxX, 7)), d -> "ok");
            row("getValues(box x after)", command(9, 2, fields(box, boxX)), Debugger::values);
            row("invokeMethod(box twice 4)", command(9, 6,
                    invokeOn(box, main, boxType, boxMethods.get("twice"), 4)), Debugger::invokeResult);
            row("invokeMethod(box, a static method)", command(9, 6,
                    invokeOn(box, main, boxType, boxMethods.get("add"), 4, 5)), Debugger::invokeResult);
            row("isCollected(box)", command(9, 9, new Payload().id(box).bytes()), Debugger::bool);
            row("isCollected(unknown id)", command(9, 9, new Payload().id(UNKNOWN).bytes()), Debugger::bool);
            row("disableCollection(unknown id)", command(9, 7, new Payload().id(UNKNOWN).bytes()), d -> "ok");
            row("enableCollection(unknown id)", command(9, 8, new Payload().id(UNKNOWN).bytes()), d -> "ok");

            System.out.println("== StringReference");
            row("value(str)", command(10, 1, new Payload().id(str).bytes()), Debugger::string);
            row("value(an int[])", command(10, 1, new Payload().id(ints).bytes()), Debugger::string);
            row("value(unknown id)", command(10, 1, new Payload().id(UNKNOWN).bytes()), Debugger::string);

            System.out.println("== ClassObjectReference");
            row("reflectedType(Integer.class)", command(17, 1, new Payload().id(cls).bytes()),
                    d -> "tag " + d.readUnsignedByte());
            row("reflectedType(unknown id)", command(17, 1, new Payload().id(UNKNOWN).bytes()),
                    d -> "tag " + d.readUnsignedByte());

            System.out.println("== ClassLoaderReference");
            row("visibleClasses(app loader) has Box", command(14, 1, new Payload().id(loader).bytes()),
                    Debugger::hasBox);
            row("visibleClasses(a string)", command(14, 1, new Payload().id(str).bytes()), Debugger::hasBox);
            row("visibleClasses(unknown id)", command(14, 1, new Payload().id(UNKNOWN).bytes()), Debugger::hasBox);

            System.out.println("== ModuleReference");
            row("name(java.base)", command(18, 1, new Payload().id(base).bytes()), Debugger::string);
            row("name(unnamed)", command(18, 1, new Payload().id(unnamed).bytes()), Debugger::string);
            row("name(unknown id)", command(18, 1, new Payload().id(UNKNOWN).bytes()), Debugger::string);
            row("classLoader(java.base)", command(18, 2, new Payload().id(base).bytes()), Debugger::nullOrNot);
            row("classLoader(unnamed)", command(18, 2, new Payload().id(unnamed).bytes()), Debugger::nullOrNot);
            row("module(probe class) is unnamed", command(2, 19, new Payload().id(type).bytes()),
                    d -> d.readLong() == unnamed ? "yes" : "no");

            System.out.println("== ForceEarlyReturn");
            row("forceEarlyReturn(running other)", command(11, 14, force(other, 'I', 1)), d -> "ok");
            row("forceEarlyReturn(unknown thread)", command(11, 14, force(UNKNOWN, 'I', 1)), d -> "ok");
            row("forceEarlyReturn(main, J)", command(11, 14, force(main, 'J', 1)), d -> "ok");
            row("forceEarlyReturn(main, V)", command(11, 14, force(main, 'V', 0)), d -> "ok");
            row("forceEarlyReturn(main, L)", command(11, 14, force(main, 'L', obj)), d -> "ok");
            row("forceEarlyReturn(main, Z)", command(11, 14, force(main, 'Z', 1)), d -> "ok");
            row("forceEarlyReturn(main, I 1)", command(11, 14, force(main, 'I', 1)), d -> "ok");

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

        static String bool(java.io.DataInputStream d) throws Exception {
            return d.readBoolean() ? "true" : "false";
        }

        static String string(java.io.DataInputStream d) throws Exception {
            return "\"" + readString(d) + "\"";
        }

        static String nullOrNot(java.io.DataInputStream d) throws Exception {
            return d.readLong() == 0 ? "null" : "an id";
        }

        /** A tagged object reply (`ArrayType.NewInstance`): its tag and null-ness. */
        static String taggedObject(java.io.DataInputStream d) throws Exception {
            char tag = (char) d.readUnsignedByte();
            return tag + (d.readLong() == 0 ? " null" : " an id");
        }

        /** An `InvokeMethod` / `NewInstance` reply: the value and whether it threw. */
        static String invokeResult(java.io.DataInputStream d) throws Exception {
            char tag = (char) d.readUnsignedByte();
            String value;
            if (tag == 'I') {
                value = "I " + d.readInt();
            } else if (tag == 'V') {
                value = "V";
            } else {
                value = tag + (d.readLong() == 0 ? " null" : " an id");
            }
            d.readUnsignedByte();
            return value + (d.readLong() == 0 ? "" : " threw");
        }

        /** An `ArrayReference.GetValues` reply: the region's tag and values. */
        static String arrayValues(java.io.DataInputStream d) throws Exception {
            char tag = (char) d.readUnsignedByte();
            int n = d.readInt();
            StringBuilder s = new StringBuilder().append(tag).append(" [");
            for (int i = 0; i < n; i++) {
                if (i > 0) {
                    s.append(' ');
                }
                if (tag == 'I') {
                    s.append(d.readInt());
                } else {
                    s.append((char) d.readUnsignedByte());
                    s.append(d.readLong() == 0 ? " null" : " id");
                }
            }
            return s.append(']').toString();
        }

        /** An `ObjectReference.GetValues` reply: each tagged value's gist. */
        static String values(java.io.DataInputStream d) throws Exception {
            int n = d.readInt();
            StringBuilder s = new StringBuilder();
            for (int i = 0; i < n; i++) {
                char tag = (char) d.readUnsignedByte();
                s.append(tag == 'I' ? "I " + d.readInt() : tag + " ?");
            }
            return s.toString();
        }

        static String hasBox(java.io.DataInputStream d) throws Exception {
            boolean found = false;
            for (int n = d.readInt(); n > 0; n--) {
                d.readUnsignedByte();
                if (d.readLong() == boxType) {
                    found = true;
                }
            }
            return found ? "yes" : "no";
        }

        static long referenceType(long object) throws Exception {
            java.io.DataInputStream t = check(command(9, 1, new Payload().id(object).bytes()));
            t.readUnsignedByte();
            return t.readLong();
        }

        static byte[] setInt(long clazz, long field, int value) throws Exception {
            Payload p = new Payload().id(clazz);
            p.i32(1);
            p.id(field);
            p.i32(value);
            return p.bytes();
        }

        static byte[] invoke(long clazz, long thread, long method, int... args) throws Exception {
            Payload p = new Payload().id(clazz).id(thread).id(method);
            p.i32(args.length);
            for (int a : args) {
                p.u8('I');
                p.i32(a);
            }
            p.i32(0); // options
            return p.bytes();
        }

        static byte[] invokeOn(long object, long thread, long clazz, long method, int... args)
                throws Exception {
            Payload p = new Payload().id(object).id(thread).id(clazz).id(method);
            p.i32(args.length);
            for (int a : args) {
                p.u8('I');
                p.i32(a);
            }
            p.i32(0); // options
            return p.bytes();
        }

        static byte[] newArray(long arrayType, int length) throws Exception {
            Payload p = new Payload().id(arrayType);
            p.i32(length);
            return p.bytes();
        }

        static byte[] range(long array, int first, int length) throws Exception {
            Payload p = new Payload().id(array);
            p.i32(first);
            p.i32(length);
            return p.bytes();
        }

        static byte[] setInts(long array, int first, int... values) throws Exception {
            Payload p = new Payload().id(array);
            p.i32(first);
            p.i32(values.length);
            for (int v : values) {
                p.i32(v);
            }
            return p.bytes();
        }

        static byte[] fields(long object, long field) throws Exception {
            Payload p = new Payload().id(object);
            p.i32(1);
            p.id(field);
            return p.bytes();
        }

        static byte[] setField(long object, long field, int value) throws Exception {
            Payload p = new Payload().id(object);
            p.i32(1);
            p.id(field);
            p.i32(value);
            return p.bytes();
        }

        static byte[] force(long thread, char tag, long value) throws Exception {
            Payload p = new Payload().id(thread);
            p.u8(tag);
            switch (tag) {
                case 'V' -> { }
                case 'Z' -> p.u8((int) value);
                case 'I' -> p.i32((int) value);
                default -> p.id(value);
            }
            return p.bytes();
        }

        static void setStatic(long field, int value) throws Exception {
            check(command(3, 2, setInt(type, field, value))); // ClassType.SetValues
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
