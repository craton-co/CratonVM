// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 22, lane fuzz8: a deopt storm inside a hidden class's
// compiled loop whose inlined callees are the class's OWN private methods,
// before and after an ordinary class of the same class-file name is loaded in
// the same loader and in a second loader.
//
// Risk targeted: wave 21 item 1 of r21-review-w21-findings-20261019.md (deopt
// resume of an unhinted scope binds a hidden class's own name by id through
// `hidden_self_cp_answer` / `find_cp_class_for_class`), and wave 20
// (`CRATONVM_HIDDEN_SELF_CP_BY_ID`, r21-hidden-self-cp-resolves-to-same-named-
// class-20261019.md). The hidden `loop` OSR-compiles and inlines `step`
// (private static, `invokestatic <ITSELF>.step`) and `bump` (private
// instance, `invokevirtual <ITSELF>.bump`). Inside `step` the interface call
// `op.applyAsInt` sees a NEW implementation class arrive mid-loop (R21Fz8HdOp1..6,
// never loaded before), so the compiled frame deopts while an inlined self
// call is on its scope chain. A resume that binds the scope by name lands in
// the ORDINARY R21Fz8HdPayload (or the second loader's copy) once those exist:
// the per-class counts below then leak between `h1`, `plain` and `other`, or
// the accumulated value stops matching the reference.
//
// Every value is a pure function of the calls made. `ref` recomputes each
// loop's result from the arrival schedule with plain arithmetic.
//
// Run:
//   javac -d out R21Fz8HiddenDeoptStorm1.java
//   java -Xmx256m -cp out R21Fz8HiddenDeoptStorm1                     # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz8HiddenDeoptStorm1                 # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz8HiddenDeoptStorm1
//   cratonvm --nojit -Xmx256m -cp out R21Fz8HiddenDeoptStorm1
//   CRATONVM_HIDDEN_SELF_CP_BY_ID=0 cratonvm ...                      # control arm (may differ)
// HotSpot expectation (by reading JVMS 5.4.3.1 and Lookup::defineHiddenClass,
// not yet run):
//   warm match=true count=40000 icount=40000 live=1
//   storm1 match=true count=130000 icount=130000 live=4
//   plain match=true count=10 icount=10 plainSelf=true plainSeesH1=false h1SeesPlain=false h1Self=true
//   other match=true count=10 icount=10 distinct=true otherSelf=true otherSeesH1=false h1SeesOther=false
//   storm2 match=true count=220000 icount=220000 live=7 h1Self=true plain=10 other=10
//   h2 match=true count=5 h2Self=true h2SeesH1=false h1SeesH2=false
//   final h1=220000 h2=5 plain=10 other=10 arrivals=6

import java.io.InputStream;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.function.IntUnaryOperator;

public class R21Fz8HiddenDeoptStorm1 {
    static final IntUnaryOperator[] OPS = new IntUnaryOperator[8];
    static int live = 1;
    static int arrivals;

    static {
        OPS[0] = new R21Fz8HdOp0();
    }

    public static IntUnaryOperator op(int i) {
        return OPS[i % live];
    }

    // Called from inside the hidden loop: the next implementation class is
    // loaded and becomes a receiver of the compiled `op.apply` site.
    public static void arrive() {
        switch (live) {
            case 1: OPS[1] = new R21Fz8HdOp1(); break;
            case 2: OPS[2] = new R21Fz8HdOp2(); break;
            case 3: OPS[3] = new R21Fz8HdOp3(); break;
            case 4: OPS[4] = new R21Fz8HdOp4(); break;
            case 5: OPS[5] = new R21Fz8HdOp5(); break;
            case 6: OPS[6] = new R21Fz8HdOp6(); break;
            default: return;
        }
        live++;
        arrivals++;
    }

    static int applyRef(int k, int x) {
        return x * (k + 1) + k;
    }

    static long ref(int n, int at, int liveBefore) {
        long acc = 0;
        int lv = liveBefore;
        for (int i = 0; i < n; i++) {
            acc += applyRef(i % lv, i ^ 1) & 0xFFFF;
            if (i == at && lv < 7) {
                lv++;
            }
        }
        return acc;
    }

    static final class Child extends ClassLoader {
        final byte[] b;

        Child(byte[] b) {
            super(R21Fz8HiddenDeoptStorm1.class.getClassLoader());
            this.b = b;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (!name.equals("R21Fz8HdPayload")) {
                return super.loadClass(name, resolve);
            }
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c == null) {
                    c = defineClass(name, b, 0, b.length);
                }
                return c;
            }
        }
    }

    static final class H {
        final Class<?> c;
        final Object obj;
        final MethodHandle loop;
        final MethodHandle count;
        final MethodHandle icount;
        final MethodHandle self;
        final MethodHandle isSelf;

        H(MethodHandles.Lookup l) throws Throwable {
            c = l.lookupClass();
            obj = l.findConstructor(c, MethodType.methodType(void.class)).invoke();
            loop = l.findVirtual(c, "loop", MethodType.methodType(long.class, int.class, int.class));
            count = l.findStatic(c, "count", MethodType.methodType(int.class));
            icount = l.findVirtual(c, "icount", MethodType.methodType(int.class));
            self = l.findStatic(c, "self", MethodType.methodType(Class.class));
            isSelf = l.findStatic(c, "isSelf", MethodType.methodType(boolean.class, Object.class));
        }

        boolean run(int n, int at) throws Throwable {
            int lb = live;
            long r = (long) loop.invoke(obj, n, at);
            return r == ref(n, at, lb);
        }

        int count() throws Throwable {
            return (int) count.invoke();
        }

        int icount() throws Throwable {
            return (int) icount.invoke(obj);
        }

        boolean selfOk() throws Throwable {
            return (Class<?>) self.invoke() == c;
        }

        boolean sees(H o) throws Throwable {
            return (boolean) isSelf.invoke(o.obj);
        }
    }

    public static void main(String[] args) throws Throwable {
        byte[] bytes;
        try (InputStream in = R21Fz8HiddenDeoptStorm1.class.getResourceAsStream("/R21Fz8HdPayload.class")) {
            bytes = in.readAllBytes();
        }
        MethodHandles.Lookup lk = MethodHandles.lookup();
        H h1 = new H(lk.defineHiddenClass(bytes, true));

        boolean ok = h1.run(40000, -1);
        System.out.println("warm match=" + ok + " count=" + h1.count() + " icount=" + h1.icount()
                + " live=" + live);

        ok = true;
        for (int k = 0; k < 3; k++) {
            ok &= h1.run(30000, 15000);
        }
        System.out.println("storm1 match=" + ok + " count=" + h1.count() + " icount=" + h1.icount()
                + " live=" + live);

        // The ordinary class of the same name, same loader (from the class path).
        H plain = new H(MethodHandles.privateLookupIn(R21Fz8HdPayload.class, lk));
        ok = plain.run(10, -1);
        System.out.println("plain match=" + ok + " count=" + plain.count() + " icount=" + plain.icount()
                + " plainSelf=" + plain.selfOk() + " plainSeesH1=" + plain.sees(h1)
                + " h1SeesPlain=" + h1.sees(plain) + " h1Self=" + h1.selfOk());

        // The same bytes as an ordinary class in a second loader.
        Class<?> oc = Class.forName("R21Fz8HdPayload", true, new Child(bytes));
        H other = new H(MethodHandles.privateLookupIn(oc, lk));
        ok = other.run(10, -1);
        System.out.println("other match=" + ok + " count=" + other.count() + " icount=" + other.icount()
                + " distinct=" + (oc != R21Fz8HdPayload.class) + " otherSelf=" + other.selfOk()
                + " otherSeesH1=" + other.sees(h1) + " h1SeesOther=" + h1.sees(other));

        // The storm again, with both names taken.
        ok = true;
        for (int k = 0; k < 3; k++) {
            ok &= h1.run(30000, 15000);
        }
        System.out.println("storm2 match=" + ok + " count=" + h1.count() + " icount=" + h1.icount()
                + " live=" + live + " h1Self=" + h1.selfOk() + " plain=" + plain.count()
                + " other=" + other.count());

        H h2 = new H(lk.defineHiddenClass(bytes, true));
        ok = h2.run(5, -1);
        System.out.println("h2 match=" + ok + " count=" + h2.count() + " h2Self=" + h2.selfOk()
                + " h2SeesH1=" + h2.sees(h1) + " h1SeesH2=" + h1.sees(h2));

        System.out.println("final h1=" + h1.count() + " h2=" + h2.count() + " plain=" + plain.count()
                + " other=" + other.count() + " arrivals=" + arrivals);
    }
}

// The operator classes are reached only through the public IntUnaryOperator
// and the public statics above, so the second loader's copy of the payload
// (another runtime package) links against them without an access error.
class R21Fz8HdOp0 implements IntUnaryOperator {
    public int applyAsInt(int x) {
        return x * 1 + 0;
    }
}

class R21Fz8HdOp1 implements IntUnaryOperator {
    public int applyAsInt(int x) {
        return x * 2 + 1;
    }
}

class R21Fz8HdOp2 implements IntUnaryOperator {
    public int applyAsInt(int x) {
        return x * 3 + 2;
    }
}

class R21Fz8HdOp3 implements IntUnaryOperator {
    public int applyAsInt(int x) {
        return x * 4 + 3;
    }
}

class R21Fz8HdOp4 implements IntUnaryOperator {
    public int applyAsInt(int x) {
        return x * 5 + 4;
    }
}

class R21Fz8HdOp5 implements IntUnaryOperator {
    public int applyAsInt(int x) {
        return x * 6 + 5;
    }
}

class R21Fz8HdOp6 implements IntUnaryOperator {
    public int applyAsInt(int x) {
        return x * 7 + 6;
    }
}

// The bytes of the hidden classes, and the ordinary class of the same name.
// No descriptor names the class itself: only its own CONSTANT_Class entry
// (this_class, which JVMS resolves to the hidden class) refers to it.
class R21Fz8HdPayload {
    static int sCount;
    int iCount;

    private int bump(int x) {
        iCount++;
        return x ^ 1;
    }

    private static int step(IntUnaryOperator op, int x) {
        sCount++;
        return op.applyAsInt(x) & 0xFFFF;
    }

    static boolean isSelf(Object o) {
        return o instanceof R21Fz8HdPayload;
    }

    static Class<?> self() {
        return R21Fz8HdPayload.class;
    }

    static int count() {
        return sCount;
    }

    int icount() {
        return iCount;
    }

    long loop(int n, int at) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            IntUnaryOperator op = R21Fz8HiddenDeoptStorm1.op(i);
            acc += step(op, bump(i));
            if (i == at) {
                R21Fz8HiddenDeoptStorm1.arrive();
            }
        }
        return acc;
    }
}
