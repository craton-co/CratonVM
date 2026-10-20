// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 84, lane L4: ONE handle adapted to two types in
// turn. The JDK's `asType` caches one adapter per handle (`asTypeCache`), so
// every call of such an alternation runs `asTypeUncached` on the SAME
// receiver. Under
//
//   CRATONVM_MH_REAL_AS_TYPE_MEMO=1  (--jdk-only, default off)
//
// wave 82 never promoted such a key (it wanted a second, different receiver);
// wave 84 promotes it for that receiver only (`PromotedSelf`) once its own
// second answer was exactly the filed prediction. Every row must still carry
// the right receiver, convert the right way, refuse a reference cast with
// the JDK's exception, and keep the JDK's cache identities: two `asType`s in
// a row answer one adapter; after another type is cached, the first type
// answers a NEW adapter, which is then cached.
//
// Rows: a bound getter (`Species_LL`), a bound int (`insertArguments`,
// `Species_LI`), a direct handle (reinvoker), a bound reference cast (a fresh
// `MH_cast.bindTo(String)` per answer: refused, never minted), and three
// identity rounds over one bound handle (round 0 files, round 1 promotes
// self and mints, round 2 mints both types).
//
// Run (all must print the block below):
//   javac -d . L4W84AsTypeMemoSameReceiver.java
//   CRATONVM_MH_REAL_AS_TYPE_MEMO=1 cratonvm --jdk-only --nojit -cp . L4W84AsTypeMemoSameReceiver
//   CRATONVM_MH_REAL_AS_TYPE_MEMO=1 cratonvm --jdk-only         -cp . L4W84AsTypeMemoSameReceiver
//   cratonvm --jdk-only --nojit -cp . L4W84AsTypeMemoSameReceiver      (switch off)
//   cratonvm --compatible       -cp . L4W84AsTypeMemoSameReceiver      (control)
//
// Expected output derived from MethodHandle.asType / asTypeCached /
// setAsTypeCache and MethodHandleImpl.makePairwiseConvertByEditor.
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// alt-bound#0 type=()Object value=5
// alt-bound#1 type=()long value=5
// alt-bound#2 type=()Object value=5
// alt-bound#3 type=()long value=5
// alt-bound#4 type=()Object value=5
// alt-bound#5 type=()long value=5
// alt-insert#0 type=(int)Object value=42
// alt-insert#1 type=(int)long value=42
// alt-insert#2 type=(int)Object value=42
// alt-insert#3 type=(int)long value=42
// alt-insert#4 type=(int)Object value=42
// alt-insert#5 type=(int)long value=42
// alt-direct#0 type=(Box)Object value=1
// alt-direct#1 type=(Box)long value=2
// alt-direct#2 type=(Box)Object value=3
// alt-direct#3 type=(Box)long value=4
// alt-direct#4 type=(Box)Object value=5
// alt-direct#5 type=(Box)long value=6
// alt-cast#0 type=(Object)Object value=y9 bad=java.lang.ClassCastException
// alt-cast#1 type=(Object)String value=y9 bad=java.lang.ClassCastException
// alt-cast#2 type=(Object)Object value=y9 bad=java.lang.ClassCastException
// alt-cast#3 type=(Object)String value=y9 bad=java.lang.ClassCastException
// alt-cast#4 type=(Object)Object value=y9 bad=java.lang.ClassCastException
// alt-cast#5 type=(Object)String value=y9 bad=java.lang.ClassCastException
// identity#0 type=()Object same=true fresh-after-other=true cached=true other=()long value=7 other-value=7
// identity#1 type=()Object same=true fresh-after-other=true cached=true other=()long value=7 other-value=7
// identity#2 type=()Object same=true fresh-after-other=true cached=true other=()long value=7 other-value=7
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W84AsTypeMemoSameReceiver {
    public static class Box {
        public int v;

        public Box(int v) {
            this.v = v;
        }

        public int a() {
            return v + 1;
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
        MethodType toObject = MethodType.methodType(Object.class);
        MethodType toLong = MethodType.methodType(long.class);

        MethodHandle getter = lk.findGetter(Box.class, "v", int.class);
        MethodHandle bound = getter.bindTo(new Box(5));
        for (int i = 0; i < 6; i++) {
            if ((i & 1) == 0) {
                MethodHandle h = bound.asType(toObject);
                Object r = (Object) h.invokeExact();
                System.out.println("alt-bound#" + i + " type=" + h.type() + " value=" + r);
            } else {
                MethodHandle h = bound.asType(toLong);
                long r = (long) h.invokeExact();
                System.out.println("alt-bound#" + i + " type=" + h.type() + " value=" + r);
            }
        }

        MethodHandle add = lk.findStatic(Box.class, "add",
                MethodType.methodType(int.class, int.class, int.class));
        MethodHandle ins = MethodHandles.insertArguments(add, 0, 40);
        MethodType intToObject = MethodType.methodType(Object.class, int.class);
        MethodType intToLong = MethodType.methodType(long.class, int.class);
        for (int i = 0; i < 6; i++) {
            if ((i & 1) == 0) {
                MethodHandle h = ins.asType(intToObject);
                Object r = (Object) h.invokeExact(2);
                System.out.println("alt-insert#" + i + " type=" + h.type() + " value=" + r);
            } else {
                MethodHandle h = ins.asType(intToLong);
                long r = (long) h.invokeExact(2);
                System.out.println("alt-insert#" + i + " type=" + h.type() + " value=" + r);
            }
        }

        MethodHandle direct = lk.findVirtual(Box.class, "a", MethodType.methodType(int.class));
        MethodType boxToObject = MethodType.methodType(Object.class, Box.class);
        MethodType boxToLong = MethodType.methodType(long.class, Box.class);
        for (int i = 0; i < 6; i++) {
            Box box = new Box(i);
            if ((i & 1) == 0) {
                MethodHandle h = direct.asType(boxToObject);
                Object r = (Object) h.invokeExact(box);
                System.out.println("alt-direct#" + i + " type=" + h.type() + " value=" + r);
            } else {
                MethodHandle h = direct.asType(boxToLong);
                long r = (long) h.invokeExact(box);
                System.out.println("alt-direct#" + i + " type=" + h.type() + " value=" + r);
            }
        }

        MethodHandle echo = lk.findVirtual(Box.class, "echo",
                MethodType.methodType(String.class, String.class)).bindTo(new Box(9));
        MethodType objToObj = MethodType.methodType(Object.class, Object.class);
        MethodType objToString = MethodType.methodType(String.class, Object.class);
        for (int i = 0; i < 6; i++) {
            String value;
            String bad;
            MethodHandle h;
            if ((i & 1) == 0) {
                h = echo.asType(objToObj);
                value = (String) (Object) h.invokeExact((Object) "y");
                try {
                    Object r = (Object) h.invokeExact((Object) Integer.valueOf(1));
                    bad = "no exception " + r;
                } catch (Throwable t) {
                    bad = t.getClass().getName();
                }
            } else {
                h = echo.asType(objToString);
                value = (String) h.invokeExact((Object) "y");
                try {
                    String r = (String) h.invokeExact((Object) Integer.valueOf(1));
                    bad = "no exception " + r;
                } catch (Throwable t) {
                    bad = t.getClass().getName();
                }
            }
            System.out.println("alt-cast#" + i + " type=" + h.type() + " value=" + value + " bad=" + bad);
        }

        MethodHandle seven = getter.bindTo(new Box(7));
        for (int round = 0; round < 3; round++) {
            MethodHandle x1 = seven.asType(toObject);
            MethodHandle x2 = seven.asType(toObject);
            MethodHandle other = seven.asType(toLong);
            MethodHandle x3 = seven.asType(toObject);
            MethodHandle x4 = seven.asType(toObject);
            Object value = (Object) x3.invokeExact();
            long otherValue = (long) other.invokeExact();
            System.out.println("identity#" + round + " type=" + x1.type() + " same=" + (x1 == x2)
                    + " fresh-after-other=" + (x3 != x1) + " cached=" + (x4 == x3)
                    + " other=" + other.type() + " value=" + value + " other-value=" + otherValue);
        }
    }
}
