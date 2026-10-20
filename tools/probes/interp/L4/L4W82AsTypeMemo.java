// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 82, lane L4: a converting `asType` of a FRESH
// handle answers what the JDK's own `asTypeUncached` answers, whether or not
//
//   CRATONVM_MH_REAL_AS_TYPE_MEMO=1  (--jdk-only) `asTypeUncached` on a
//       direct or bound handle mints the adapter from a filed answer once
//       two answers for the same (receiver class, type, form, new type)
//       agreed word for word.
//
// Each row runs six rounds on a NEW receiver per round (round 0 files a
// candidate, round 1 promotes it, rounds 2-5 are minted), and every answer
// must carry ITS receiver: a bound getter over six boxes, two different
// methods of one type alternating (one key: same class, type and form), two
// static methods alternating, a reference-cast adapter (a fresh cast handle
// per answer: never promoted), and a bound int (`insertArguments`: the bound
// primitive is the receiver's). Plus the JDK's own cache: `asType` twice on
// one handle answers the same adapter, two fresh handles two adapters.
//
// Run (all must print the block below): --jdk-only --nojit and --jdk-only
// (JIT), each with and without the switch, and --compatible (control).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// bound#0 value=0
// bound#1 value=10
// bound#2 value=20
// bound#3 value=30
// bound#4 value=40
// bound#5 value=50
// direct#0 value=1
// direct#1 value=3
// direct#2 value=3
// direct#3 value=5
// direct#4 value=5
// direct#5 value=7
// static#0 value=11
// static#1 value=22
// static#2 value=11
// static#3 value=22
// static#4 value=11
// static#5 value=22
// cast#0 value=x0 bad=java.lang.ClassCastException
// cast#1 value=x1 bad=java.lang.ClassCastException
// cast#2 value=x2 bad=java.lang.ClassCastException
// cast#3 value=x3 bad=java.lang.ClassCastException
// cast#4 value=x4 bad=java.lang.ClassCastException
// cast#5 value=x5 bad=java.lang.ClassCastException
// insert#0 value=100
// insert#1 value=101
// insert#2 value=102
// insert#3 value=103
// insert#4 value=104
// insert#5 value=105
// cache#0 type=()Object same=true fresh-differs=true value=7
// cache#1 type=()Object same=true fresh-differs=true value=7
// cache#2 type=()Object same=true fresh-differs=true value=7
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W82AsTypeMemo {
    public static class Box {
        public int v;

        public Box(int v) {
            this.v = v;
        }

        public int a() {
            return v + 1;
        }

        public int b() {
            return v + 2;
        }

        public static int s1() {
            return 11;
        }

        public static int s2() {
            return 22;
        }

        public String echo(String x) {
            return x + v;
        }

        public static int add(int x, int y) {
            return x + y;
        }
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lk = MethodHandles.lookup();
        MethodType mtI = MethodType.methodType(int.class);
        MethodHandle getter = lk.findGetter(Box.class, "v", int.class);
        for (int i = 0; i < 6; i++) {
            Object value = getter.bindTo(new Box(10 * i)).invoke();
            System.out.println("bound#" + i + " value=" + value);
        }
        MethodType toObject = MethodType.methodType(Object.class, Box.class);
        for (int i = 0; i < 6; i++) {
            MethodHandle h = lk.findVirtual(Box.class, (i & 1) == 0 ? "a" : "b", mtI);
            Object value = (Object) h.asType(toObject).invokeExact(new Box(i));
            System.out.println("direct#" + i + " value=" + value);
        }
        for (int i = 0; i < 6; i++) {
            MethodHandle h = lk.findStatic(Box.class, (i & 1) == 0 ? "s1" : "s2", mtI);
            Object value = (Object) h.asType(MethodType.methodType(Object.class)).invokeExact();
            System.out.println("static#" + i + " value=" + value);
        }
        MethodType echoType = MethodType.methodType(String.class, String.class);
        MethodType looseEcho = MethodType.methodType(Object.class, Box.class, Object.class);
        for (int i = 0; i < 6; i++) {
            MethodHandle h = lk.findVirtual(Box.class, "echo", echoType).asType(looseEcho);
            Box box = new Box(i);
            Object value = (Object) h.invokeExact(box, (Object) "x");
            String bad;
            try {
                Object r = (Object) h.invokeExact(box, (Object) Integer.valueOf(1));
                bad = "no exception " + r;
            } catch (Throwable t) {
                bad = t.getClass().getName();
            }
            System.out.println("cast#" + i + " value=" + value + " bad=" + bad);
        }
        MethodHandle add = lk.findStatic(Box.class, "add", MethodType.methodType(int.class, int.class, int.class));
        MethodType intToObject = MethodType.methodType(Object.class, int.class);
        for (int i = 0; i < 6; i++) {
            MethodHandle ins = MethodHandles.insertArguments(add, 0, i);
            Object value = (Object) ins.asType(intToObject).invokeExact(100);
            System.out.println("insert#" + i + " value=" + value);
        }
        MethodType objectOnly = MethodType.methodType(Object.class);
        Box seven = new Box(7);
        for (int i = 0; i < 3; i++) {
            MethodHandle bound = getter.bindTo(seven);
            MethodHandle t1 = bound.asType(objectOnly);
            MethodHandle t2 = bound.asType(objectOnly);
            MethodHandle other = getter.bindTo(seven).asType(objectOnly);
            Object value = (Object) t1.invokeExact();
            System.out.println("cache#" + i + " type=" + t1.type() + " same=" + (t1 == t2)
                    + " fresh-differs=" + (other != t1) + " value=" + value);
        }
    }
}
