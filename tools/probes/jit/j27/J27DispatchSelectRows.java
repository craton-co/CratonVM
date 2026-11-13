// j27 wave 5 (lane INVHUNT): JVMS 5.4.6 / 6.5 selection rows for compiled
// call sites. Each row warms a call site with ordinary receivers until it
// compiles, then hands it the tricky receiver (or one defined before the warm
// loop and kept out of the profile, so a class-hierarchy guard admits it):
//
//   watermark-abstract-redecl  invokevirtual G.m on a Leaf below an ABSTRACT
//                              re-declaration (J27SelS): AbstractMethodError.
//                              Exercises CRATONVM_JIT_IR_CHA_MONOMORPHIC_SPLICE=1
//                              (page j27-cha-watermark-survey-reads-the-lenient-lookup).
//   super-abstract-redecl      a spliced body's super.m() whose superclass
//                              re-declares m abstract: AbstractMethodError
//                              (CRATONVM_JIT_IR_SUPER_SPECIAL_SPLICE, default ON;
//                              page j27-super-special-splice-walks-past-an-abstract-redeclaration).
//   super-ancestor-cp-class    (lane INVSP) a spliced body's invokespecial
//                              whose constant-pool class is an ANCESTOR above
//                              the direct superclass (J27AncD extends J27AncB
//                              extends G, names G.m; javac never emits it):
//                              ACC_SUPER selects J27AncB.m (7), never G.m (1).
//                              The single-pass nested splice
//                              (CRATONVM_JIT_INLINE_NEST, default ON) and the
//                              optimizing tier both reach it through callSupAnc.
//   pkg-private-cross-package  q.J27PkgB.m does NOT override p.J27PkgA.m
//                              (package-private): A.m runs every time.
//   private-nestmate           invokevirtual of a PRIVATE nestmate method; a
//                              subclass's public namesake does not override it.
//   conflict-default           invokeinterface on a class inheriting two
//                              unrelated defaults: IncompatibleClassChangeError.
//   miranda                    invokevirtual through an abstract class that
//                              names the interface method only (miranda).
//   ic-late-override           a monomorphic site meets an overriding subclass
//                              loaded after it compiled.
//   hidden-late-override       the same with a hidden class
//                              (Lookup.defineHiddenClass) as the subclass.
//   sigpoly-wrong-type         invokeExact at a warm site with a handle of
//                              another type: WrongMethodTypeException.
//
// The separately compiled shapes (J27SelS, J27SelLeaf, J27SelSuper, J27AncB, J27AncD,
// p/J27PkgA, q/J27PkgB, p/J27PkgCaller, J27SelConflict, the hidden subclass)
// are written by the small class writer below (version 52, no branches, so
// no stack maps). Every `fact` is a computed value, a count, or an exception
// class; the `-msg` facts carry the message cut at the first " (".
//
//   java -cp <dir> J27DispatchSelectRows                       > hs.txt
//   cratonvm --cp <dir> J27DispatchSelectRows | diff - hs.txt
//   CRATONVM_JIT_IR_CHA_MONOMORPHIC_SPLICE=1 cratonvm --cp <dir> J27DispatchSelectRows | diff - hs.txt
//   (and with --compatible, and once with --nojit)
//
// Expected HotSpot 25 output. The counts and exception classes are certain
// from the JVMS; the -msg texts were derived from HotSpot 25 sources
// (linkResolver.cpp, defaultMethods.cpp, Invokers.java) and
// vm/src/runtime/resolve/selection.rs, not from a run: the first host `java`
// run is the reference and must be diffed against these lines.
//   fact watermark-abstract-redecl java.lang.AbstractMethodError thrown=1000 sum=0
//   fact watermark-abstract-redecl-msg Receiver class J27SelLeaf does not define or inherit an implementation of the resolved method 'int m()' of class J27DispatchSelectRows$G. Selected method is 'abstract int J27SelS.m()'.
//   (j27 wave 9, lane W9EXC, measured on 25.0.3: default, C1-only and C2-only `java`; `java -Xint` and
//   `cratonvm --nojit` answer the linked vtable-entry text instead:
//   Missing implementation of resolved method 'abstract int m()' of abstract class J27SelS.)
//   fact super-abstract-redecl java.lang.AbstractMethodError thrown=201000 sum=0
//   fact super-abstract-redecl-msg 'int J27SelS.m()'
//   fact super-ancestor-cp-class 1400000
//   fact pkg-private-cross-package 200000
//   fact private-nestmate 200000
//   fact conflict-default java.lang.IncompatibleClassChangeError thrown=1000 sum=0
//   fact conflict-default-msg Conflicting default methods: J27DispatchSelectRows$DA.d J27DispatchSelectRows$DB.d
//   fact miranda 600000
//   fact ic-late-override 2000
//   fact hidden-late-override 50000
//   fact sigpoly-wrong-type java.lang.invoke.WrongMethodTypeException
//   fact sigpoly-wrong-type-msg handle's method type (int)long but found (int)int
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.HashMap;
import java.util.Map;

public class J27DispatchSelectRows {
    static final int WARM = 200_000;
    static final int LATE = 1000;

    // ---- watermark / super-special: G.m, re-declared abstract by J27SelS ----
    public static class G { public int m() { return 1; } }
    public static class G1 extends G { }
    public static class G2 extends G { }
    public static class G3 extends G { }

    static int callG(G g) { return g.m(); }

    public interface Sup { int sup(); }

    static int callSup(Sup s) { return s.sup(); }

    /** Its own site, so its receiver profile names J27AncD alone. */
    static int callSupAnc(Sup s) { return s.sup(); }

    // ---- package-private across packages ------------------------------------
    public interface PkgCall { int call(Object a); }

    // ---- private nestmate ---------------------------------------------------
    static class NestBase { private int p() { return 1; } }
    static final class NestSub extends NestBase { public int p() { return 2; } }

    static int callNest(NestBase n) { return n.p(); }

    // ---- conflicting defaults -----------------------------------------------
    public interface DA { default int d() { return 1; } }
    public interface DB { default int d() { return 2; } }
    static final class DaOnly1 implements DA { }
    static final class DaOnly2 implements DA { }

    static int callD(DA a) { return a.d(); }

    // ---- miranda ------------------------------------------------------------
    public interface Fn { int f(); }
    abstract static class Mir implements Fn { }
    static final class MirA extends Mir { public int f() { return 1; } }
    static final class MirB extends Mir { public int f() { return 2; } }

    static int callMir(Mir m) { return m.f(); }

    // ---- late overriders ----------------------------------------------------
    public static class IcA { public int v() { return 1; } }
    /** Loaded only through Class.forName, after the site compiled. */
    public static class IcB extends IcA { public int v() { return 2; } }

    static int callIc(IcA a) { return a.v(); }

    public static class HBase { public int h() { return 1; } }

    static int callH(HBase b) { return b.h(); }

    // ---- signature polymorphic ----------------------------------------------
    static int intId(int x) { return x; }
    static long longId(int x) { return x; }

    static int callExact(MethodHandle h, int x) throws Throwable { return (int) h.invokeExact(x); }

    public static void main(String[] args) throws Throwable {
        Bytes loader = new Bytes();
        watermarkAbstract(loader);
        superAbstract(loader);
        superAncestor(loader);
        pkgPrivate(loader);
        privateNestmate();
        conflictDefault(loader);
        miranda();
        icLateOverride();
        hiddenLateOverride();
        sigPoly();
    }

    static void watermarkAbstract(Bytes loader) throws Exception {
        // Defined BEFORE the warm loop: below any class-id watermark, and
        // never in the site's profile.
        loader.define("J27SelS", ClassWriter52.abstractRedeclaration());
        G leaf = (G) loader.define("J27SelLeaf", ClassWriter52.plain("J27SelLeaf", "J27SelS"))
                .getConstructor().newInstance();
        G[] warm = {new G(), new G1(), new G2(), new G3()};
        long ignored = 0;
        for (int i = 0; i < WARM; i++) {
            ignored += callG(warm[i & 3]);
        }
        lateThrows("watermark-abstract-redecl", () -> callG(leaf));
    }

    static void superAbstract(Bytes loader) throws Exception {
        Sup s = (Sup) loader.define("J27SelSuper", ClassWriter52.superCaller())
                .getConstructor().newInstance();
        // Every call throws; the warm loop is what compiles callSup.
        int thrown = 0;
        long sum = 0;
        String msg = "none";
        for (int i = 0; i < WARM + LATE; i++) {
            try {
                sum += callSup(s);
            } catch (AbstractMethodError e) {
                thrown++;
                msg = message(e);
            }
        }
        fact("super-abstract-redecl", "java.lang.AbstractMethodError thrown=" + thrown + " sum=" + sum);
        fact("super-abstract-redecl-msg", msg);
    }

    static void superAncestor(Bytes loader) throws Exception {
        loader.define("J27AncB", ClassWriter52.ancestorMiddle());
        Sup d = (Sup) loader.define("J27AncD", ClassWriter52.ancestorCaller())
                .getConstructor().newInstance();
        long sum = 0;
        for (int i = 0; i < WARM; i++) {
            sum += callSupAnc(d);
        }
        fact("super-ancestor-cp-class", Long.toString(sum));
    }

    static void pkgPrivate(Bytes loader) throws Exception {
        loader.define("p.J27PkgA", ClassWriter52.pkgA());
        Object b = loader.define("q.J27PkgB", ClassWriter52.pkgB())
                .getConstructor().newInstance();
        PkgCall caller = (PkgCall) loader.define("p.J27PkgCaller", ClassWriter52.pkgCaller())
                .getConstructor().newInstance();
        long sum = 0;
        for (int i = 0; i < WARM; i++) {
            sum += caller.call(b);
        }
        fact("pkg-private-cross-package", Long.toString(sum));
    }

    static void privateNestmate() {
        NestBase n = new NestSub();
        long sum = 0;
        for (int i = 0; i < WARM; i++) {
            sum += callNest(n);
        }
        fact("private-nestmate", Long.toString(sum));
    }

    static void conflictDefault(Bytes loader) throws Exception {
        DA conflict = (DA) loader.define("J27SelConflict", ClassWriter52.conflict())
                .getConstructor().newInstance();
        DA[] warm = {new DaOnly1(), new DaOnly2()};
        long ignored = 0;
        for (int i = 0; i < WARM; i++) {
            ignored += callD(warm[i & 1]);
        }
        lateThrows("conflict-default", () -> callD(conflict));
    }

    static void miranda() {
        Mir[] arr = {new MirA(), new MirB()};
        long sum = 0;
        for (int i = 0; i < 2 * WARM; i++) {
            sum += callMir(arr[i & 1]);
        }
        fact("miranda", Long.toString(sum));
    }

    static void icLateOverride() throws Exception {
        IcA a = new IcA();
        long ignored = 0;
        for (int i = 0; i < WARM; i++) {
            ignored += callIc(a);
        }
        IcA b = (IcA) Class.forName("J27DispatchSelectRows$IcB")
                .getDeclaredConstructor().newInstance();
        long sum = 0;
        for (int i = 0; i < LATE; i++) {
            sum += callIc(b);
        }
        fact("ic-late-override", Long.toString(sum));
    }

    static void hiddenLateOverride() throws Throwable {
        HBase a = new HBase();
        long ignored = 0;
        for (int i = 0; i < WARM; i++) {
            ignored += callH(a);
        }
        Class<?> hidden = MethodHandles.lookup()
                .defineHiddenClass(ClassWriter52.hiddenSub(), true).lookupClass();
        HBase h = (HBase) hidden.getConstructor().newInstance();
        long sum = 0;
        for (int i = 0; i < LATE; i++) {
            sum += callH(h);
        }
        fact("hidden-late-override", Long.toString(sum));
    }

    static void sigPoly() throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodHandle ii = l.findStatic(J27DispatchSelectRows.class, "intId",
                MethodType.methodType(int.class, int.class));
        MethodHandle il = l.findStatic(J27DispatchSelectRows.class, "longId",
                MethodType.methodType(long.class, int.class));
        long ignored = 0;
        for (int i = 0; i < WARM; i++) {
            ignored += callExact(ii, i);
        }
        try {
            callExact(il, 1);
            fact("sigpoly-wrong-type", "no-error");
        } catch (Throwable e) {
            fact("sigpoly-wrong-type", e.getClass().getName());
            fact("sigpoly-wrong-type-msg", message(e));
        }
    }

    interface IntCall { int call() throws Throwable; }

    /** LATE calls that must each throw; the count, the class and the text. */
    static void lateThrows(String name, IntCall call) {
        int thrown = 0;
        long sum = 0;
        String cls = "none";
        String msg = "none";
        for (int i = 0; i < LATE; i++) {
            try {
                sum += call.call();
            } catch (Throwable e) {
                thrown++;
                cls = e.getClass().getName();
                msg = message(e);
            }
        }
        fact(name, cls + " thrown=" + thrown + " sum=" + sum);
        fact(name + "-msg", msg);
    }

    static String message(Throwable e) {
        String msg = String.valueOf(e.getMessage());
        int cut = msg.indexOf(" (");
        return cut < 0 ? msg : msg.substring(0, cut);
    }

    static void fact(String name, String value) {
        System.out.println("fact " + name + " " + value);
    }

    /** Defines the version-52 classes; its parent sees this probe's classes. */
    static final class Bytes extends ClassLoader {
        Bytes() {
            super(J27DispatchSelectRows.class.getClassLoader());
        }

        Class<?> define(String name, byte[] bytes) {
            return defineClass(name, bytes, 0, bytes.length);
        }
    }

    /** A minimal version-52 class-file writer: no fields, no stack maps. */
    static final class ClassWriter52 {
        static final String OBJECT = "java/lang/Object";
        static final String PROBE = "J27DispatchSelectRows";
        static final int PUBLIC_SUPER = 0x0021;
        static final int PUBLIC_SUPER_ABSTRACT = 0x0421;

        private final ByteArrayOutputStream cpBytes = new ByteArrayOutputStream();
        private final DataOutputStream cp = new DataOutputStream(cpBytes);
        private final Map<String, Integer> pool = new HashMap<>();
        private int next = 1;
        private final ByteArrayOutputStream methodBytes = new ByteArrayOutputStream();
        private final DataOutputStream methods = new DataOutputStream(methodBytes);
        private int methodCount;

        /** `abstract class J27SelS extends G { public abstract int m(); }`. */
        static byte[] abstractRedeclaration() throws IOException {
            ClassWriter52 w = new ClassWriter52();
            w.constructor(PROBE + "$G");
            w.abstractMethod("m", "()I");
            return w.bytes(PUBLIC_SUPER_ABSTRACT, "J27SelS", PROBE + "$G");
        }

        /**
         * `class J27SelSuper extends J27SelS implements Sup { public int m()
         * { return 100; } public int sup() { return super.m(); } }`.
         */
        static byte[] superCaller() throws IOException {
            ClassWriter52 w = new ClassWriter52();
            w.constructor("J27SelS");
            w.method(0x0001, "m", "()I", 1, 1, code(0x10, 100, 0xac)); // bipush 100; ireturn
            int sm = w.ref(10, "J27SelS", "m", "()I");
            w.method(0x0001, "sup", "()I", 1, 1,
                     code(0x2a, 0xb7, sm >> 8, sm & 0xff, 0xac)); // aload_0; invokespecial; ireturn
            return w.bytes(PUBLIC_SUPER, "J27SelSuper", "J27SelS", PROBE + "$Sup");
        }

        /** `class J27AncB extends G { public int m() { return 7; } }`. */
        static byte[] ancestorMiddle() throws IOException {
            ClassWriter52 w = new ClassWriter52();
            w.constructor(PROBE + "$G");
            w.method(0x0001, "m", "()I", 1, 1, code(0x10, 7, 0xac)); // bipush 7; ireturn
            return w.bytes(PUBLIC_SUPER, "J27AncB", PROBE + "$G");
        }

        /**
         * `class J27AncD extends J27AncB implements Sup { public int m()
         * { return 100; } public int sup() { invokespecial G.m } }`: the
         * constant pool names G, two levels up (ACC_SUPER is set).
         */
        static byte[] ancestorCaller() throws IOException {
            ClassWriter52 w = new ClassWriter52();
            w.constructor("J27AncB");
            w.method(0x0001, "m", "()I", 1, 1, code(0x10, 100, 0xac)); // bipush 100; ireturn
            int gm = w.ref(10, PROBE + "$G", "m", "()I");
            w.method(0x0001, "sup", "()I", 1, 1,
                     code(0x2a, 0xb7, gm >> 8, gm & 0xff, 0xac)); // aload_0; invokespecial; ireturn
            return w.bytes(PUBLIC_SUPER, "J27AncD", "J27AncB", PROBE + "$Sup");
        }

        /** `package p; public class J27PkgA { int m() { return 1; } }`. */
        static byte[] pkgA() throws IOException {
            ClassWriter52 w = new ClassWriter52();
            w.constructor(OBJECT);
            w.method(0x0000, "m", "()I", 1, 1, code(0x04, 0xac)); // iconst_1; ireturn
            return w.bytes(PUBLIC_SUPER, "p/J27PkgA", OBJECT);
        }

        /** `package q; public class J27PkgB extends p.J27PkgA { public int m() { return 2; } }`. */
        static byte[] pkgB() throws IOException {
            ClassWriter52 w = new ClassWriter52();
            w.constructor("p/J27PkgA");
            w.method(0x0001, "m", "()I", 1, 1, code(0x05, 0xac)); // iconst_2; ireturn
            return w.bytes(PUBLIC_SUPER, "q/J27PkgB", "p/J27PkgA");
        }

        /** `package p; class J27PkgCaller implements PkgCall { call(o) { return ((J27PkgA) o).m(); } }`. */
        static byte[] pkgCaller() throws IOException {
            ClassWriter52 w = new ClassWriter52();
            w.constructor(OBJECT);
            int a = w.cls("p/J27PkgA");
            int m = w.ref(10, "p/J27PkgA", "m", "()I");
            w.method(0x0001, "call", "(Ljava/lang/Object;)I", 1, 2,
                     code(0x2b, 0xc0, a >> 8, a & 0xff, 0xb6, m >> 8, m & 0xff, 0xac));
            return w.bytes(PUBLIC_SUPER, "p/J27PkgCaller", OBJECT, PROBE + "$PkgCall");
        }

        /** `class J27SelConflict implements DA, DB { }` (javac refuses it). */
        static byte[] conflict() throws IOException {
            ClassWriter52 w = new ClassWriter52();
            w.constructor(OBJECT);
            return w.bytes(PUBLIC_SUPER, "J27SelConflict", OBJECT, PROBE + "$DA", PROBE + "$DB");
        }

        /** A class with only a public no-argument constructor. */
        static byte[] plain(String name, String superName) throws IOException {
            ClassWriter52 w = new ClassWriter52();
            w.constructor(superName);
            return w.bytes(PUBLIC_SUPER, name, superName);
        }

        /** The hidden `J27DispatchSelectRows$HSub extends HBase { public int h() { return 50; } }`. */
        static byte[] hiddenSub() throws IOException {
            ClassWriter52 w = new ClassWriter52();
            w.constructor(PROBE + "$HBase");
            w.method(0x0001, "h", "()I", 1, 1, code(0x10, 50, 0xac)); // bipush 50; ireturn
            return w.bytes(PUBLIC_SUPER, PROBE + "$HSub", PROBE + "$HBase");
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

        void abstractMethod(String name, String desc) throws IOException {
            methods.writeShort(0x0401); // ACC_PUBLIC | ACC_ABSTRACT
            methods.writeShort(utf8(name));
            methods.writeShort(utf8(desc));
            methods.writeShort(0); // no Code
            methodCount++;
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

        byte[] bytes(int access, String name, String superName, String... interfaces)
                throws IOException {
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
            o.writeShort(access);
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
