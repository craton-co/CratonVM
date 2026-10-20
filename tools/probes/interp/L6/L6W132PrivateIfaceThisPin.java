// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 132, lane L6: stage 1 of
// docs/internal/proposals/interpreter/i40-L2-proposal-guard-a-pinned-private-interface-call-instead-of-refusing-the-method-20261004.md
// (`CRATONVM_JIT_PRIVATE_IFACE_THIS_PIN`, default OFF). A method holding an
// `invokeinterface` of a PRIVATE interface method is refused by the compile
// doors (`invokeinterface-private-receiver-unchecked`); with the switch on it
// compiles when every such receiver is the method's unstored `this`.
//
//   named-group      MatchResult.start/end/group(String) on toMatchResult()
//                    (JDK defaults calling the private groupNumber)
//   user-default     a default method calling two private helpers on `this`
//   user-long        a private helper taking a long built from ints (the
//                    SegmentAllocator.allocateFrom(String) shape)
//   other-receiver   a default method calling a private helper on ANOTHER
//                    instance (receiver is local 1: never proven, stays refused)
//   allocate-from    SegmentAllocator.allocateFrom(JAVA_INT, int) on an arena
//   special-good     hand bytecode: default m(Object) does `invokespecial
//                    priv` on its argument, priv does `invokeinterface priv2`
//                    on `this`; argument implements the interface
//   special-bad      the same with a String argument: HotSpot raises
//                    IncompatibleClassChangeError at the invokespecial
//
// Each row runs its method hot (compiled on HotSpot). TIME lines are per-op
// timings and are excluded from comparisons.
//
// special-bad is a known DIFF with the switch OFF too: CratonVM does not make
// HotSpot's interface-sender invokespecial receiver check (page
// docs/known-issues/interpreter/i132-L6-interface-invokespecial-skips-the-receiver-subtype-check-20261230.md).
// It is the row that decides the flip: while that check is missing, `this` of
// an interface method can be a foreign object, and with the switch on the
// compiled `priv` runs `priv2` where the interpreter raises.
//
// Run: javac -d out L6W132PrivateIfaceThisPin.java
//      cratonvm [--compatible] [--nojit] -cp out L6W132PrivateIfaceThisPin
//      CRATONVM_JIT_PRIVATE_IFACE_THIS_PIN=1 cratonvm [--compatible] [--nojit] -cp out L6W132PrivateIfaceThisPin
//
// HotSpot 25.0.3, default and -Xint (identical):
// HOTSPOT_EXPECTED_BEGIN
// rep 0
// named-group sum=2000000
// user-default sum=3399999
// user-long span=7233333
// other-receiver sum=400000
// allocate-from sum=119800000
// rep 1
// named-group sum=2000000
// user-default sum=3399999
// user-long span=7233333
// other-receiver sum=400000
// allocate-from sum=119800000
// rep 2
// named-group sum=2000000
// user-default sum=3399999
// user-long span=7233333
// other-receiver sum=400000
// allocate-from sum=119800000
// special-good: ok=30000 icce=0 other=0
// special-bad: ok=0 icce=30000 other=0 first=java.lang.IllegalAccessError: Receiver class java.lang.String must be the current class or a subtype of interface W132Sp
// HOTSPOT_EXPECTED_END
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.HashMap;
import java.util.Map;
import java.util.regex.MatchResult;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

public class L6W132PrivateIfaceThisPin {
    static final int N = 200_000;

    interface Shape {
        int side();

        default int area2() {
            return twice(side()) + bump(3);
        }

        default long span(int a, int b) {
            return widen((long) a + b);
        }

        default int peer(Shape other) {
            return other.twice(1);
        }

        private int twice(int x) {
            return x * 2;
        }

        private int bump(int k) {
            return side() + k;
        }

        private long widen(long v) {
            return v * 3 + side();
        }
    }

    static final class Sq implements Shape {
        final int s;

        Sq(int s) {
            this.s = s;
        }

        public int side() {
            return s;
        }
    }

    static final class Tri implements Shape {
        public int side() {
            return 7;
        }
    }

    static void time(String row, long t0, int ops) {
        System.out.println("TIME " + row + " ns/op=" + (System.nanoTime() - t0) / Math.max(1, ops));
    }

    static void namedGroup() {
        Pattern p = Pattern.compile("(?<k>[a-z]+)=(?<v>[0-9]+)");
        Matcher m = p.matcher("");
        String[] inputs = {"alpha=12", "be=345", "gamma=6", "d=7890"};
        long sum = 0;
        long t0 = System.nanoTime();
        for (int i = 0; i < N; i++) {
            m.reset(inputs[i & 3]);
            if (!m.find()) {
                continue;
            }
            MatchResult r = m.toMatchResult();
            sum += r.start("v") + r.end("k") + r.group("v").length();
        }
        time("named-group", t0, N);
        System.out.println("named-group sum=" + sum);
    }

    static void userDefault() {
        Shape[] shapes = {new Sq(2), new Tri(), new Sq(5)};
        long sum = 0;
        long span = 0;
        long t0 = System.nanoTime();
        for (int i = 0; i < N; i++) {
            Shape s = shapes[i % 3];
            sum += s.area2();
            span += s.span(i & 15, 3);
        }
        time("user-default", t0, N);
        System.out.println("user-default sum=" + sum);
        System.out.println("user-long span=" + span);
        long peer = 0;
        for (int i = 0; i < N; i++) {
            peer += shapes[i % 3].peer(shapes[(i + 1) % 3]);
        }
        System.out.println("other-receiver sum=" + peer);
    }

    static void allocateFrom() {
        long sum = 0;
        long t0 = System.nanoTime();
        try {
            for (int round = 0; round < N / 1000; round++) {
                try (Arena arena = Arena.ofConfined()) {
                    for (int i = 0; i < 1000; i++) {
                        MemorySegment seg = arena.allocateFrom(ValueLayout.JAVA_INT, i + round);
                        sum += seg.get(ValueLayout.JAVA_INT, 0);
                    }
                }
            }
            time("allocate-from", t0, N);
            System.out.println("allocate-from sum=" + sum);
        } catch (Throwable t) {
            System.out.println("allocate-from threw " + t.getClass().getName());
        }
    }

    static final class Loader extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();

        Loader() {
            super(L6W132PrivateIfaceThisPin.class.getClassLoader());
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            byte[] b = bytes.get(name);
            if (b == null) {
                throw new ClassNotFoundException(name);
            }
            return defineClass(name, b, 0, b.length);
        }
    }

    static final MethodTypeDesc STR = MethodTypeDesc.of(ConstantDescs.CD_String);
    static final MethodTypeDesc OBJ_STR = MethodTypeDesc.of(ConstantDescs.CD_String, ConstantDescs.CD_Object);

    static byte[] specialIface(String name) {
        ClassDesc self = ClassDesc.of(name);
        return ClassFile.of().build(self, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_INTERFACE | ClassFile.ACC_ABSTRACT);
            cb.withMethodBody("m", OBJ_STR, ClassFile.ACC_PUBLIC,
                    code -> code.aload(1).invokespecial(self, "priv", STR, true).areturn());
            cb.withMethodBody("priv", STR, ClassFile.ACC_PRIVATE,
                    code -> code.aload(0).invokeinterface(self, "priv2", STR).areturn());
            cb.withMethodBody("priv2", STR, ClassFile.ACC_PRIVATE, code -> code.ldc("p2").areturn());
        });
    }

    static byte[] specialImpl(String name, String iface) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withInterfaceSymbols(ClassDesc.of(iface));
            cb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0).invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                            ConstantDescs.MTD_void).return_());
        });
    }

    static void special() throws Throwable {
        Loader loader = new Loader();
        loader.bytes.put("W132Sp", specialIface("W132Sp"));
        loader.bytes.put("W132SpImpl", specialImpl("W132SpImpl", "W132Sp"));
        Class<?> iface = Class.forName("W132Sp", true, loader);
        Object impl = Class.forName("W132SpImpl", true, loader).getConstructor().newInstance();
        MethodHandle m = MethodHandles.lookup().findVirtual(iface, "m",
                MethodType.methodType(String.class, Object.class));
        for (int phase = 0; phase < 2; phase++) {
            String row = phase == 0 ? "special-good" : "special-bad";
            Object arg = phase == 0 ? impl : "not-an-impl";
            int ok = 0;
            int icce = 0;
            int other = 0;
            String firstMessage = null;
            String firstOther = null;
            for (int i = 0; i < 30_000; i++) {
                try {
                    String r = (String) m.invoke(impl, arg);
                    if ("p2".equals(r)) {
                        ok++;
                    } else {
                        other++;
                        if (firstOther == null) {
                            firstOther = "result " + r;
                        }
                    }
                } catch (IncompatibleClassChangeError e) {
                    icce++;
                    if (firstMessage == null) {
                        firstMessage = e.getClass().getName() + ": " + e.getMessage();
                    }
                } catch (Throwable t) {
                    other++;
                    if (firstOther == null) {
                        firstOther = t.getClass().getName();
                    }
                }
            }
            System.out.println(row + ": ok=" + ok + " icce=" + icce + " other=" + other
                    + (firstMessage == null ? "" : " first=" + firstMessage)
                    + (firstOther == null ? "" : " firstOther=" + firstOther));
        }
    }

    public static void main(String[] args) throws Throwable {
        for (int rep = 0; rep < 3; rep++) {
            System.out.println("rep " + rep);
            namedGroup();
            userDefault();
            allocateFrom();
        }
        special();
    }
}
