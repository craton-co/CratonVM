// j27 wave 4 (lane DISPROWS): the JIT dispatch / IC / door rows of
// docs/feature-designs/jit-ir-optimizations.md
// ("JIT dispatch / IC / doors"): megamorphic virtual and interface sites, a
// receiver class loaded after the site warmed (CHA and megamorphic), default
// methods (and a default diamond), a private interface method reached through
// invokeinterface, a version-52 subclass whose PRIVATE method hides nothing
// from an invokevirtual site (MEGA2-17-3), AbstractMethodError and
// IncompatibleClassChangeError from cold and warm sites, and helpful
// NullPointerException messages from warm sites.
//
// The version-52 classes (J27DispB, J27DispNoApply, J27DispNotOp,
// J27DispCaller, J27DispCallerCold) are written by the small class writer
// below and defined in a child loader, so one source file yields the
// separately-compiled shapes javac refuses to produce.
//
// Every `fact` row is a computed value or an exception class + message (cut
// at the first " (", where HotSpot appends loader identities with an identity
// hash); none depends on timing or on which tier ran.
//
//   java -cp <dir> J27DispatchRows                  > hs.txt
//   cratonvm --cp <dir> J27DispatchRows | diff - hs.txt
//   (and with --compatible, and once with --nojit)
// Timing (separate `time` lines, ns/op, median of 5 reps):
//   java -Dj27.timing=true -cp <dir> J27DispatchRows
//
// Expected HotSpot 25 output (facts only). The AME / ICCE / NPE texts were
// derived from HotSpot 25 sources (linkResolver.cpp, interpreterRuntime.cpp,
// bytecodeUtils.cpp), not from a run: the first host `java` run is the
// reference and must be diffed against these lines.
//   fact mega-virtual 900000
//   fact late-virtual 136000
//   fact mega-interface 1100000
//   fact late-interface 144000
//   fact cha-warm 1400000
//   fact cha-late 11
//   fact cha-mixed 18000
//   fact default-mega 1650000
//   fact default-diamond DB 200000
//   fact private-iface 2300000
//   fact private-hiding-cold Base
//   fact private-hiding-warm base=100000 sub1=100000
//   fact ame-cold java.lang.AbstractMethodError Receiver class J27DispNoApply does not define or inherit an implementation of the resolved method 'abstract int apply(int)' of interface J27DispatchRows$Op.
//   fact ame-warm java.lang.AbstractMethodError Receiver class J27DispNoApply does not define or inherit an implementation of the resolved method 'abstract int apply(int)' of interface J27DispatchRows$Op.
//   fact icce-cold java.lang.IncompatibleClassChangeError Class J27DispNotOp does not implement the requested interface J27DispatchRows$Op
//   fact icce-warm-sum 400000
//   fact icce-warm java.lang.IncompatibleClassChangeError Class J27DispNotOp does not implement the requested interface J27DispatchRows$Op
// (icce-cold is the RESOLVING execution of a fresh call site
// (LinkResolver::runtime_resolve_interface_method), icce-warm an itable miss
// on a resolved, warmed site; HotSpot builds a message in each place, so a
// difference between the two host lines is HotSpot's, not a probe bug. The
// text above is the one vm/src/runtime/resolve/selection.rs
// `receiver_does_not_implement` already matches.)
//   fact npe-virtual java.lang.NullPointerException Cannot invoke "J27DispatchRows$Shape.area()" because "J27DispatchRows.CUR_SHAPE" is null
//   fact npe-interface java.lang.NullPointerException Cannot invoke "J27DispatchRows$Op.apply(int)" because "J27DispatchRows.CUR_OP" is null
//   fact npe-default java.lang.NullPointerException Cannot invoke "J27DispatchRows$Dflt.d()" because "J27DispatchRows.CUR_DFLT" is null
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.util.Arrays;
import java.util.HashMap;
import java.util.Map;

public class J27DispatchRows {
    static final int WARM = 200_000;

    // ---- megamorphic virtual ------------------------------------------------
    public abstract static class Shape { public abstract int area(); }
    static final class S0 extends Shape { public int area() { return 1; } }
    static final class S1 extends Shape { public int area() { return 2; } }
    static final class S2 extends Shape { public int area() { return 3; } }
    static final class S3 extends Shape { public int area() { return 4; } }
    static final class S4 extends Shape { public int area() { return 5; } }
    static final class S5 extends Shape { public int area() { return 6; } }
    static final class S6 extends Shape { public int area() { return 7; } }
    static final class S7 extends Shape { public int area() { return 8; } }
    /** Loaded only through Class.forName, after the virtual site warmed. */
    static final class LateShape extends Shape { public int area() { return 100; } }

    static Shape CUR_SHAPE;

    static long shapeLoop(Shape[] arr, int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            CUR_SHAPE = arr[i % arr.length];
            sum += CUR_SHAPE.area();
        }
        return sum;
    }

    // ---- megamorphic interface ----------------------------------------------
    public interface Op { int apply(int x); }
    static final class O0 implements Op { public int apply(int x) { return x + 1; } }
    static final class O1 implements Op { public int apply(int x) { return x + 2; } }
    static final class O2 implements Op { public int apply(int x) { return x + 3; } }
    static final class O3 implements Op { public int apply(int x) { return x + 4; } }
    static final class O4 implements Op { public int apply(int x) { return x + 5; } }
    static final class O5 implements Op { public int apply(int x) { return x + 6; } }
    static final class O6 implements Op { public int apply(int x) { return x + 7; } }
    static final class O7 implements Op { public int apply(int x) { return x + 8; } }
    /** Loaded only through Class.forName, after the interface site warmed. */
    static final class LateOp implements Op { public int apply(int x) { return x + 99; } }

    static Op CUR_OP;

    static long opLoop(Op[] arr, int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            CUR_OP = arr[i % arr.length];
            sum += CUR_OP.apply(1);
        }
        return sum;
    }

    /** A site executed once: the interpreter's (HotSpot) cold door. */
    static int applyOnce(Op op) { return op.apply(1); }

    // ---- CHA: one loaded implementor, then a late one -------------------------
    public abstract static class Mono { public abstract int v(); }
    static final class MonoA extends Mono { public int v() { return 7; } }
    /** Loaded only through Class.forName, after `callMono` warmed. */
    static final class MonoB extends Mono { public int v() { return 11; } }

    static int callMono(Mono m) { return m.v(); }

    // ---- default methods --------------------------------------------------------
    public interface Dflt { default int d() { return 1; } }
    static final class D0 implements Dflt { }
    static final class D1 implements Dflt { }
    static final class D2 implements Dflt { public int d() { return 30; } }
    static final class D3 implements Dflt { }

    static Dflt CUR_DFLT;

    static long dfltLoop(Dflt[] arr, int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            CUR_DFLT = arr[i % arr.length];
            sum += CUR_DFLT.d();
        }
        return sum;
    }

    public interface DA { default String who() { return "DA"; } }
    public interface DB extends DA { default String who() { return "DB"; } }
    static final class DC implements DA, DB { }

    // ---- a private interface method through invokeinterface (javac 11+) -------
    public interface Priv {
        private int secret(int x) { return x * 3; }
        default int pub(int x) { return secret(x) + 1; }
    }
    static final class P0 implements Priv { }
    static final class P1 implements Priv { }

    // ---- MEGA2-17-3: a private method hides nothing from invokevirtual --------
    public static class Base {
        public Base() { }
        public String m() { return "Base"; }
    }
    static final class Sub1 extends Base { public String m() { return "Sub1"; } }

    static String callM(Base b) { return b.m(); }

    // ---- ICCE through a separately compiled caller ----------------------------
    public interface Caller { int call(Object o, int x); }

    public static void main(String[] args) throws Exception {
        Bytes loader = new Bytes();
        megaVirtual();
        megaInterface();
        cha();
        defaults();
        privateIface();
        privateHiding(loader);
        abstractMethodErrors(loader);
        incompatibleClassChange(loader);
        helpfulNpes();
        if (Boolean.getBoolean("j27.timing")) {
            timing();
        }
    }

    static Shape[] shapes() {
        return new Shape[] {new S0(), new S1(), new S2(), new S3(),
                            new S4(), new S5(), new S6(), new S7()};
    }

    static Op[] ops() {
        return new Op[] {new O0(), new O1(), new O2(), new O3(),
                         new O4(), new O5(), new O6(), new O7()};
    }

    static void megaVirtual() throws Exception {
        Shape[] arr = shapes();
        fact("mega-virtual", Long.toString(shapeLoop(arr, WARM)));
        Shape late = (Shape) Class.forName("J27DispatchRows$LateShape")
                .getDeclaredConstructor().newInstance();
        Shape[] nine = Arrays.copyOf(arr, 9);
        nine[8] = late;
        fact("late-virtual", Long.toString(shapeLoop(nine, 9 * 1000)));
    }

    static void megaInterface() throws Exception {
        Op[] arr = ops();
        fact("mega-interface", Long.toString(opLoop(arr, WARM)));
        Op late = (Op) Class.forName("J27DispatchRows$LateOp")
                .getDeclaredConstructor().newInstance();
        Op[] nine = Arrays.copyOf(arr, 9);
        nine[8] = late;
        // 1000 rounds of (2+3+...+9) + 100.
        fact("late-interface", Long.toString(opLoop(nine, 9 * 1000)));
    }

    static void cha() throws Exception {
        Mono a = new MonoA();
        long sum = 0;
        for (int i = 0; i < WARM; i++) {
            sum += callMono(a);
        }
        fact("cha-warm", Long.toString(sum));
        Mono b = (Mono) Class.forName("J27DispatchRows$MonoB")
                .getDeclaredConstructor().newInstance();
        fact("cha-late", Integer.toString(callMono(b)));
        long mixed = 0;
        for (int i = 0; i < 2000; i++) {
            mixed += callMono((i & 1) == 0 ? a : b);
        }
        fact("cha-mixed", Long.toString(mixed));
    }

    static void defaults() {
        Dflt[] arr = {new D0(), new D1(), new D2(), new D3()};
        fact("default-mega", Long.toString(dfltLoop(arr, WARM)));
        DA dc = new DC();
        int db = 0;
        String last = "";
        for (int i = 0; i < WARM; i++) {
            last = dc.who();
            if (last.equals("DB")) {
                db++;
            }
        }
        fact("default-diamond", last + " " + db);
    }

    static void privateIface() {
        Priv[] arr = {new P0(), new P1()};
        long sum = 0;
        for (int i = 0; i < WARM; i++) {
            sum += arr[i & 1].pub(i & 7);
        }
        fact("private-iface", Long.toString(sum));
    }

    static void privateHiding(Bytes loader) throws Exception {
        Base b = (Base) loader.define("J27DispB", ClassWriter52.privateHider())
                .getConstructor().newInstance();
        fact("private-hiding-cold", callM(b));
        Base[] arr = {new Sub1(), b};
        int base = 0;
        int sub1 = 0;
        for (int i = 0; i < WARM; i++) {
            String r = callM(arr[i & 1]);
            if (r.equals("Base")) {
                base++;
            } else if (r.equals("Sub1")) {
                sub1++;
            }
        }
        fact("private-hiding-warm", "base=" + base + " sub1=" + sub1);
    }

    static void abstractMethodErrors(Bytes loader) throws Exception {
        Op noApply = (Op) loader.define("J27DispNoApply", ClassWriter52.noApply())
                .getConstructor().newInstance();
        try {
            applyOnce(noApply);
            fact("ame-cold", "no-error");
        } catch (AbstractMethodError e) {
            fact("ame-cold", describe(e));
        }
        try {
            opLoop(new Op[] {noApply}, 1);
            fact("ame-warm", "no-error");
        } catch (AbstractMethodError e) {
            fact("ame-warm", describe(e));
        }
    }

    static void incompatibleClassChange(Bytes loader) throws Exception {
        Object notOp = loader.define("J27DispNotOp", ClassWriter52.plain("J27DispNotOp"))
                .getConstructor().newInstance();
        Caller cold = (Caller) loader.define("J27DispCallerCold",
                ClassWriter52.caller("J27DispCallerCold")).getConstructor().newInstance();
        try {
            cold.call(notOp, 1);
            fact("icce-cold", "no-error");
        } catch (IncompatibleClassChangeError e) {
            fact("icce-cold", describe(e));
        }
        Caller warm = (Caller) loader.define("J27DispCaller",
                ClassWriter52.caller("J27DispCaller")).getConstructor().newInstance();
        Op o0 = new O0();
        long sum = 0;
        for (int i = 0; i < WARM; i++) {
            sum += warm.call(o0, 1);
        }
        fact("icce-warm-sum", Long.toString(sum));
        try {
            warm.call(notOp, 1);
            fact("icce-warm", "no-error");
        } catch (IncompatibleClassChangeError e) {
            fact("icce-warm", describe(e));
        }
    }

    static void helpfulNpes() {
        try {
            shapeLoop(new Shape[] {null}, 1);
            fact("npe-virtual", "no-error");
        } catch (NullPointerException e) {
            fact("npe-virtual", describe(e));
        }
        try {
            opLoop(new Op[] {null}, 1);
            fact("npe-interface", "no-error");
        } catch (NullPointerException e) {
            fact("npe-interface", describe(e));
        }
        try {
            dfltLoop(new Dflt[] {null}, 1);
            fact("npe-default", "no-error");
        } catch (NullPointerException e) {
            fact("npe-default", describe(e));
        }
    }

    static void timing() {
        Shape[] arr = shapes();
        Op[] ops = ops();
        long[] v = new long[5];
        long[] i = new long[5];
        long sink = 0;
        for (int rep = 0; rep < 5; rep++) {
            long t0 = System.nanoTime();
            sink += shapeLoop(arr, 2_000_000);
            long t1 = System.nanoTime();
            sink += opLoop(ops, 2_000_000);
            long t2 = System.nanoTime();
            v[rep] = (t1 - t0) / 2_000_000;
            i[rep] = (t2 - t1) / 2_000_000;
        }
        Arrays.sort(v);
        Arrays.sort(i);
        System.out.println("time mega-virtual " + v[2] + " ns/op");
        System.out.println("time mega-interface " + i[2] + " ns/op");
        if (sink == 42) {
            System.out.println("sink");
        }
    }

    static String describe(Throwable e) {
        String msg = String.valueOf(e.getMessage());
        int cut = msg.indexOf(" (");
        return e.getClass().getName() + " " + (cut < 0 ? msg : msg.substring(0, cut));
    }

    static void fact(String name, String value) {
        System.out.println("fact " + name + " " + value);
    }

    /** Defines the version-52 classes; its parent sees this probe's classes. */
    static final class Bytes extends ClassLoader {
        Bytes() {
            super(J27DispatchRows.class.getClassLoader());
        }

        Class<?> define(String name, byte[] bytes) {
            return defineClass(name, bytes, 0, bytes.length);
        }
    }

    /** A minimal version-52 class-file writer: no fields, no stack maps. */
    static final class ClassWriter52 {
        static final String OBJECT = "java/lang/Object";
        static final String BASE = "J27DispatchRows$Base";
        static final String OP = "J27DispatchRows$Op";
        static final String CALLER = "J27DispatchRows$Caller";

        private final ByteArrayOutputStream cpBytes = new ByteArrayOutputStream();
        private final DataOutputStream cp = new DataOutputStream(cpBytes);
        private final Map<String, Integer> pool = new HashMap<>();
        private int next = 1;
        private final ByteArrayOutputStream methodBytes = new ByteArrayOutputStream();
        private final DataOutputStream methods = new DataOutputStream(methodBytes);
        private int methodCount;

        /** `J27DispB extends Base { public <init>; private String m() { "B-private" } }`. */
        static byte[] privateHider() throws IOException {
            ClassWriter52 w = new ClassWriter52();
            w.constructor(BASE);
            int s = w.string("B-private");
            w.method(0x0002, "m", "()Ljava/lang/String;", 1, 1,
                     code(0x13, s >> 8, s & 0xff, 0xb0)); // ldc_w; areturn
            return w.bytes("J27DispB", BASE);
        }

        /** `J27DispNoApply implements Op` that declares no `apply`. */
        static byte[] noApply() throws IOException {
            ClassWriter52 w = new ClassWriter52();
            w.constructor(OBJECT);
            return w.bytes("J27DispNoApply", OBJECT, OP);
        }

        /** A class with only a public no-argument constructor. */
        static byte[] plain(String name) throws IOException {
            ClassWriter52 w = new ClassWriter52();
            w.constructor(OBJECT);
            return w.bytes(name, OBJECT);
        }

        /**
         * `name implements Caller { public int call(Object o, int x) {
         * return ((Op) o).apply(x) } }` WITHOUT the checkcast: the
         * invokeinterface receives the raw Object, as separately compiled
         * code may.
         */
        static byte[] caller(String name) throws IOException {
            ClassWriter52 w = new ClassWriter52();
            w.constructor(OBJECT);
            int apply = w.ref(11, OP, "apply", "(I)I");
            w.method(0x0001, "call", "(Ljava/lang/Object;I)I", 2, 3,
                     code(0x2b, 0x1c, 0xb9, apply >> 8, apply & 0xff, 2, 0, 0xac));
            return w.bytes(name, OBJECT, CALLER);
        }

        void constructor(String superName) throws IOException {
            int init = ref(10, superName, "<init>", "()V");
            method(0x0001, "<init>", "()V", 1, 1,
                   code(0x2a, 0xb7, init >> 8, init & 0xff, 0xb1)); // aload_0; invokespecial; return
        }

        int utf8(String s) throws IOException {
            Integer known = pool.get("U" + s);
            if (known != null) {
                return known;
            }
            cp.writeByte(1);
            cp.writeUTF(s);
            pool.put("U" + s, next);
            return next++;
        }

        int cls(String name) throws IOException {
            Integer known = pool.get("C" + name);
            if (known != null) {
                return known;
            }
            int u = utf8(name);
            cp.writeByte(7);
            cp.writeShort(u);
            pool.put("C" + name, next);
            return next++;
        }

        int string(String s) throws IOException {
            int u = utf8(s);
            cp.writeByte(8);
            cp.writeShort(u);
            return next++;
        }

        int ref(int tag, String owner, String name, String desc) throws IOException {
            int c = cls(owner);
            int n = utf8(name);
            int d = utf8(desc);
            cp.writeByte(12); // NameAndType
            cp.writeShort(n);
            cp.writeShort(d);
            int nat = next++;
            cp.writeByte(tag); // 10 Methodref, 11 InterfaceMethodref
            cp.writeShort(c);
            cp.writeShort(nat);
            return next++;
        }

        void method(int flags, String name, String desc, int maxStack, int maxLocals,
                    byte[] code) throws IOException {
            int n = utf8(name);
            int d = utf8(desc);
            int c = utf8("Code");
            methods.writeShort(flags);
            methods.writeShort(n);
            methods.writeShort(d);
            methods.writeShort(1); // one attribute: Code
            methods.writeShort(c);
            methods.writeInt(12 + code.length);
            methods.writeShort(maxStack);
            methods.writeShort(maxLocals);
            methods.writeInt(code.length);
            methods.write(code);
            methods.writeShort(0); // exception table
            methods.writeShort(0); // Code attributes
            methodCount++;
        }

        byte[] bytes(String name, String superName, String... interfaces) throws IOException {
            int self = cls(name);
            int sup = cls(superName);
            int[] ifaces = new int[interfaces.length];
            for (int k = 0; k < interfaces.length; k++) {
                ifaces[k] = cls(interfaces[k]);
            }
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            DataOutputStream o = new DataOutputStream(out);
            o.writeInt(0xCAFEBABE);
            o.writeShort(0);
            o.writeShort(52);
            o.writeShort(next);
            o.write(cpBytes.toByteArray());
            o.writeShort(0x0021); // ACC_PUBLIC | ACC_SUPER
            o.writeShort(self);
            o.writeShort(sup);
            o.writeShort(ifaces.length);
            for (int i : ifaces) {
                o.writeShort(i);
            }
            o.writeShort(0); // fields
            o.writeShort(methodCount);
            o.write(methodBytes.toByteArray());
            o.writeShort(0); // class attributes
            return out.toByteArray();
        }

        static byte[] code(int... ops) {
            byte[] b = new byte[ops.length];
            for (int i = 0; i < ops.length; i++) {
                b[i] = (byte) ops[i];
            }
            return b;
        }
    }
}
