// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// 2026-10-18 (page
// `docs/internal/fixed-bugs/interpreter-i95-L4-a-warm-field-handle-call-runs-the-jdk-linker-per-call-FIXED-20261018.md`):
// `CRATONVM_JIT_MH_REAL_FIELD_LANE` (default ON, `=0` off) serves a compiled
// `invokeExact` / `invoke` site of a warm real field accessor, or of a proven
// `bindTo` of one, from the dispatch helper without the native funnel. This
// probe drives every field kind through warm COMPILED sites (each `loop*`
// method runs long enough to be compiled, then again) and the edges where the
// lane must decline and the ordinary route answer:
//
//   prim-*        getters and setters of every primitive kind, plain and
//                 volatile, instance and static (the raw return of each kind)
//   ref-*         reference getter / setter (String), an interface-typed
//                 field (a value of an implementing class: the lane declines)
//   bound-*       `bindTo` getter and setter
//   null-base     a null base after warm calls: checkBase's NPE
//   wrong-type    a getter called at another site type: WrongMethodTypeException
//   lazy-static   a static getter whose holder is initialized by the first call
//   gc-between    a collection between warm calls
//   two-callers   one handle from two compiled caller classes
//
// Every line is deterministic (no timings). Run in both modes, JIT and
// `--nojit`, with the switch on and `=0`: every line equals HotSpot's.
//
// Run: javac -d . L4R1018JitRealFieldLane.java
//      java -cp . L4R1018JitRealFieldLane                    (HotSpot)
//      cratonvm --jdk-only -cp . L4R1018JitRealFieldLane
//      CRATONVM_JIT_MH_REAL_FIELD_LANE=0 cratonvm --jdk-only -cp . L4R1018JitRealFieldLane
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// prim-int 399980000 19999
// prim-long 399980000 -5
// prim-double 2.4E8 2.5
// prim-float 4.0002096E7 1.5
// prim-boolean 20000 false
// prim-byte -40000 -2
// prim-char 1300000 A
// prim-short -60000 -3
// prim-volatile-long 399980000 9
// prim-static-int 399980000 11
// ref-string 20000 hello
// ref-iface run run
// bound-int 399980000 13
// null-base java.lang.NullPointerException
// wrong-type java.lang.invoke.WrongMethodTypeException
// lazy-static clinit
// lazy-static 42 42
// gc-between 140000 140000
// two-callers 140000 140000
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4R1018JitRealFieldLane {
    static final int N = 20000;

    static final class Box {
        int i;
        long j;
        double d;
        float f;
        boolean z;
        byte b;
        char c;
        short s;
        volatile long vj;
        static int si;
        String str;
        Runnable r;
    }

    static final class Lazy {
        static int x;

        static {
            System.out.println("lazy-static clinit");
            x = 42;
        }
    }

    static final MethodHandles.Lookup L = MethodHandles.lookup();

    static long loopInt(MethodHandle get, MethodHandle set, Box box) throws Throwable {
        long acc = 0;
        for (int k = 0; k < N; k++) {
            set.invokeExact(box, k);
            acc += (int) get.invokeExact(box);
            acc += (int) get.invoke(box);
        }
        return acc;
    }

    static long loopLong(MethodHandle get, MethodHandle set, Box box) throws Throwable {
        long acc = 0;
        for (int k = 0; k < N; k++) {
            set.invokeExact(box, (long) k);
            acc += (long) get.invokeExact(box) * 2;
        }
        return acc;
    }

    static double loopDouble(MethodHandle get, MethodHandle set, Box box) throws Throwable {
        double acc = 0;
        for (int k = 0; k < N; k++) {
            set.invokeExact(box, k + 0.5);
            acc += (double) get.invokeExact(box) * 1.2;
        }
        return acc;
    }

    static float loopFloat(MethodHandle get, MethodHandle set, Box box) throws Throwable {
        float acc = 0;
        for (int k = 0; k < N; k++) {
            set.invokeExact(box, 1000.5f);
            acc += (float) get.invokeExact(box);
            set.invokeExact(box, 1.5f);
            acc += (float) get.invokeExact(box);
            acc -= 1001.5f;
            acc += 2000.0f;
        }
        return acc;
    }

    static int loopBoolean(MethodHandle get, MethodHandle set, Box box) throws Throwable {
        int acc = 0;
        for (int k = 0; k < N; k++) {
            set.invokeExact(box, (k & 1) == 0);
            if ((boolean) get.invokeExact(box)) {
                acc += 2;
            }
        }
        return acc;
    }

    static int loopByte(MethodHandle get, MethodHandle set, Box box) throws Throwable {
        int acc = 0;
        for (int k = 0; k < N; k++) {
            set.invokeExact(box, (byte) -2);
            acc += (byte) get.invokeExact(box);
        }
        return acc;
    }

    static int loopChar(MethodHandle get, MethodHandle set, Box box) throws Throwable {
        int acc = 0;
        for (int k = 0; k < N; k++) {
            set.invokeExact(box, 'A');
            acc += (char) get.invokeExact(box);
        }
        return acc;
    }

    static int loopShort(MethodHandle get, MethodHandle set, Box box) throws Throwable {
        int acc = 0;
        for (int k = 0; k < N; k++) {
            set.invokeExact(box, (short) -3);
            acc += (short) get.invokeExact(box);
        }
        return acc;
    }

    static long loopVolatile(MethodHandle get, MethodHandle set, Box box) throws Throwable {
        long acc = 0;
        for (int k = 0; k < N; k++) {
            set.invokeExact(box, (long) k);
            acc += (long) get.invokeExact(box) * 2;
        }
        return acc;
    }

    static long loopStatic(MethodHandle get, MethodHandle set) throws Throwable {
        long acc = 0;
        for (int k = 0; k < N; k++) {
            set.invokeExact(k);
            acc += (int) get.invokeExact() * 2;
        }
        return acc;
    }

    static int loopString(MethodHandle get, MethodHandle set, Box box, String v) throws Throwable {
        int acc = 0;
        for (int k = 0; k < N; k++) {
            set.invokeExact(box, v);
            if ((String) get.invokeExact(box) == v) {
                acc++;
            }
        }
        return acc;
    }

    static int loopIface(MethodHandle get, MethodHandle set, Box box, Runnable v) throws Throwable {
        int acc = 0;
        for (int k = 0; k < N; k++) {
            set.invokeExact(box, v);
            if ((Runnable) get.invokeExact(box) == v) {
                acc++;
            }
        }
        return acc;
    }

    static long loopBound(MethodHandle get, MethodHandle set) throws Throwable {
        long acc = 0;
        for (int k = 0; k < N; k++) {
            set.invokeExact(k);
            acc += (int) get.invokeExact() * 2;
        }
        return acc;
    }

    static final class Other {
        static long run(MethodHandle get, Box box) throws Throwable {
            long acc = 0;
            for (int k = 0; k < N; k++) {
                acc += (int) get.invokeExact(box);
            }
            return acc;
        }
    }

    static long loopGet(MethodHandle get, Box box) throws Throwable {
        long acc = 0;
        for (int k = 0; k < N; k++) {
            acc += (int) get.invokeExact(box);
        }
        return acc;
    }

    static MethodHandle g(String name, Class<?> t) throws Exception {
        return L.findGetter(Box.class, name, t);
    }

    static MethodHandle s(String name, Class<?> t) throws Exception {
        return L.findSetter(Box.class, name, t);
    }

    public static void main(String[] args) throws Throwable {
        Box box = new Box();
        long r = 0;
        for (int rep = 0; rep < 3; rep++) {
            r = loopInt(g("i", int.class), s("i", int.class), box);
        }
        System.out.println("prim-int " + r + " " + (int) g("i", int.class).invokeExact(box));
        long lr = 0;
        for (int rep = 0; rep < 3; rep++) {
            lr = loopLong(g("j", long.class), s("j", long.class), box);
        }
        box.j = -5;
        System.out.println("prim-long " + lr + " " + (long) g("j", long.class).invokeExact(box));
        double dr = 0;
        for (int rep = 0; rep < 3; rep++) {
            dr = loopDouble(g("d", double.class), s("d", double.class), box);
        }
        box.d = 2.5;
        System.out.println("prim-double " + dr + " " + (double) g("d", double.class).invokeExact(box));
        float fr = 0;
        for (int rep = 0; rep < 3; rep++) {
            fr = loopFloat(g("f", float.class), s("f", float.class), box);
        }
        System.out.println("prim-float " + fr + " " + (float) g("f", float.class).invokeExact(box));
        int zr = 0;
        for (int rep = 0; rep < 3; rep++) {
            zr = loopBoolean(g("z", boolean.class), s("z", boolean.class), box);
        }
        System.out.println("prim-boolean " + zr + " " + (boolean) g("z", boolean.class).invokeExact(box));
        int br = 0;
        for (int rep = 0; rep < 3; rep++) {
            br = loopByte(g("b", byte.class), s("b", byte.class), box);
        }
        System.out.println("prim-byte " + br + " " + (byte) g("b", byte.class).invokeExact(box));
        int cr = 0;
        for (int rep = 0; rep < 3; rep++) {
            cr = loopChar(g("c", char.class), s("c", char.class), box);
        }
        System.out.println("prim-char " + cr + " " + (char) g("c", char.class).invokeExact(box));
        int sr = 0;
        for (int rep = 0; rep < 3; rep++) {
            sr = loopShort(g("s", short.class), s("s", short.class), box);
        }
        System.out.println("prim-short " + sr + " " + (short) g("s", short.class).invokeExact(box));
        long vr = 0;
        for (int rep = 0; rep < 3; rep++) {
            vr = loopVolatile(g("vj", long.class), s("vj", long.class), box);
        }
        box.vj = 9;
        System.out.println("prim-volatile-long " + vr + " " + (long) g("vj", long.class).invokeExact(box));
        MethodHandle sg = L.findStaticGetter(Box.class, "si", int.class);
        MethodHandle ss = L.findStaticSetter(Box.class, "si", int.class);
        long str = 0;
        for (int rep = 0; rep < 3; rep++) {
            str = loopStatic(sg, ss);
        }
        Box.si = 11;
        System.out.println("prim-static-int " + str + " " + (int) sg.invokeExact());
        String hello = "hello";
        int rs = 0;
        for (int rep = 0; rep < 3; rep++) {
            rs = loopString(g("str", String.class), s("str", String.class), box, hello);
        }
        System.out.println("ref-string " + rs + " " + (String) g("str", String.class).invokeExact(box));
        Runnable task = new Runnable() {
            public void run() {
            }

            public String toString() {
                return "run";
            }
        };
        int ri = 0;
        for (int rep = 0; rep < 3; rep++) {
            ri = loopIface(g("r", Runnable.class), s("r", Runnable.class), box, task);
        }
        System.out.println("ref-iface " + (ri == N ? "run" : "bad") + " " + (Runnable) g("r", Runnable.class).invokeExact(box));
        MethodHandle bg = g("i", int.class).bindTo(box);
        MethodHandle bs = s("i", int.class).bindTo(box);
        long bl = 0;
        for (int rep = 0; rep < 3; rep++) {
            bl = loopBound(bg, bs);
        }
        box.i = 13;
        System.out.println("bound-int " + bl + " " + (int) bg.invokeExact());
        MethodHandle gi = g("i", int.class);
        for (int rep = 0; rep < 3; rep++) {
            loopGet(gi, box);
        }
        try {
            loopGet(gi, null);
            System.out.println("null-base no exception");
        } catch (NullPointerException e) {
            System.out.println("null-base " + e.getClass().getName());
        }
        try {
            long w = (long) gi.invokeExact(box);
            System.out.println("wrong-type no exception " + w);
        } catch (java.lang.invoke.WrongMethodTypeException e) {
            System.out.println("wrong-type " + e.getClass().getName());
        }
        MethodHandle lazy = L.findStaticGetter(Lazy.class, "x", int.class);
        int lz = (int) lazy.invokeExact();
        long lzsum = 0;
        for (int k = 0; k < N; k++) {
            lzsum += (int) lazy.invokeExact();
        }
        System.out.println("lazy-static " + lz + " " + lzsum / N);
        box.i = 7;
        long before = loopGet(gi, box);
        System.gc();
        long after = loopGet(gi, box);
        System.out.println("gc-between " + before + " " + after);
        long o1 = 0;
        long o2 = 0;
        for (int rep = 0; rep < 3; rep++) {
            o1 = loopGet(gi, box);
            o2 = Other.run(gi, box);
        }
        System.out.println("two-callers " + o1 + " " + o2);
    }
}
