// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 64, lane L4 -- an instance field handle carries
// its slot from mint (`CRATONVM_MH_FIELD_SLOT_FROM_MINT`, opt-in, on top of
// `CRATONVM_MH_OWNER_ID_AUTHORITY`; proposal
// `i63-L4-proposal-field-handles-carry-their-slot-from-mint-20261027.md`).
//
// A pure performance change: every row must print the same with and without
// the switches, in both modes, and match HotSpot. The rows cover what a slot
// resolved once at mint could get wrong: a field inherited from a superclass
// through a subclass `refc`, a HIDDEN field (`Sub.v` hides `Base.v`: each
// handle reads its own owner's slot on a `Sub` / `Deep` receiver), fields of
// every width (int, long, double, reference, boolean) so the slots differ,
// `unreflectGetter` / `unreflectSetter` of a private field, `asType` copies
// (the copy keeps the minted slot), `bindTo`, the field-accessor link made
// for one receiver class and relinked for another (the link keeps the minted
// slot), and the casts and the null check still made after the warm loop.
// The static field rows are controls (no minted slot).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// a-base: 1
// a-via-sub: 1
// a-via-sub-on-deep: 1
// b-long: 2
// c-ref: c
// d-private: 4.5
// e-boolean: true
// f-deep: 6
// v-base-on-sub: 10
// v-sub-on-sub: 20
// v-sub-on-deep: 20
// v-base-on-deep: 10
// set-v-base-on-sub: 11/20
// set-v-sub-on-deep: 10/21
// set-b-long: 9
// set-c-ref: z
// set-d-private: 5.5
// set-e-boolean: false
// astype-v-sub: 20
// astype-astype-v-base: 10
// astype-setter-v-sub: 10/33
// bindTo-v-sub: 20
// bindTo-setter-a: 44
// static-s: 7
// static-set-s: 8
// warm: 50000
// warm-setter: 30/40/41
// after-v-sub-on-sub: 20
// after-v-sub-on-deep: 30
// after-exact-string: java.lang.ClassCastException: Cannot cast java.lang.String to L4W64FieldSlotFromMint$Sub
// after-exact-base: java.lang.ClassCastException: Cannot cast L4W64FieldSlotFromMint$Base to L4W64FieldSlotFromMint$Sub
// after-invoke-string: java.lang.ClassCastException: Cannot cast java.lang.String to L4W64FieldSlotFromMint$Sub
// after-invoke-null: java.lang.NullPointerException
// after-setter-null: java.lang.NullPointerException
// HOTSPOT_EXPECTED_END
//
// Switches: `CRATONVM_MH_OWNER_ID_AUTHORITY=1 CRATONVM_MH_FIELD_SLOT_FROM_MINT=1`
// (and stage 1's `CRATONVM_MH_INSTANCE_OWNER_ID`, default on). Under
// `--jdk-only` the finders answer the JDK's own `DirectMethodHandle`s, so the
// switch reaches only this model's handles (`--compatible`); run both modes
// anyway, with and without the switches: no row may move.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L4W64FieldSlotFromMint`.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Field;

public class L4W64FieldSlotFromMint {
    public static class Base {
        public static int s = 7;
        public int a = 1;
        public long b = 2L;
        public String c = "c";
        public int v = 10;
        private double d = 4.5;
    }

    public static class Sub extends Base {
        public int v = 20;
        public boolean e = true;
    }

    public static class Deep extends Sub {
        public int f = 6;
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = String.valueOf(t);
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lk = MethodHandles.lookup();
        MethodHandle aBase = lk.findGetter(Base.class, "a", int.class);
        MethodHandle aSub = lk.findGetter(Sub.class, "a", int.class);
        MethodHandle bGet = lk.findGetter(Base.class, "b", long.class);
        MethodHandle cGet = lk.findGetter(Base.class, "c", String.class);
        Field dField = Base.class.getDeclaredField("d");
        dField.setAccessible(true);
        MethodHandle dGet = lk.unreflectGetter(dField);
        MethodHandle dSet = lk.unreflectSetter(dField);
        MethodHandle eGet = lk.findGetter(Sub.class, "e", boolean.class);
        MethodHandle fGet = lk.findGetter(Deep.class, "f", int.class);
        MethodHandle vBase = lk.findGetter(Base.class, "v", int.class);
        MethodHandle vSub = lk.findGetter(Sub.class, "v", int.class);
        MethodHandle vBaseSet = lk.findSetter(Base.class, "v", int.class);
        MethodHandle vSubSet = lk.unreflectSetter(Sub.class.getField("v"));
        MethodHandle aSet = lk.findSetter(Base.class, "a", int.class);
        MethodHandle bSet = lk.findSetter(Base.class, "b", long.class);
        MethodHandle cSet = lk.findSetter(Base.class, "c", String.class);
        MethodHandle eSet = lk.findSetter(Sub.class, "e", boolean.class);
        MethodHandle sGet = lk.findStaticGetter(Base.class, "s", int.class);
        MethodHandle sSet = lk.findStaticSetter(Base.class, "s", int.class);

        Object base = new Base();
        Object sub = new Sub();
        Object deep = new Deep();

        row("a-base", () -> (int) aBase.invoke(base));
        row("a-via-sub", () -> (int) aSub.invoke(sub));
        row("a-via-sub-on-deep", () -> (int) aSub.invoke(deep));
        row("b-long", () -> (long) bGet.invoke(base));
        row("c-ref", () -> (String) cGet.invoke(base));
        row("d-private", () -> (double) dGet.invoke(base));
        row("e-boolean", () -> (boolean) eGet.invoke(sub));
        row("f-deep", () -> (int) fGet.invoke(deep));
        row("v-base-on-sub", () -> (int) vBase.invoke(sub));
        row("v-sub-on-sub", () -> (int) vSub.invoke(sub));
        row("v-sub-on-deep", () -> (int) vSub.invoke(deep));
        row("v-base-on-deep", () -> (int) vBase.invoke(deep));
        row("set-v-base-on-sub", () -> {
            Object o = new Sub();
            vBaseSet.invoke(o, 11);
            return (int) vBase.invoke(o) + "/" + (int) vSub.invoke(o);
        });
        row("set-v-sub-on-deep", () -> {
            Object o = new Deep();
            vSubSet.invoke(o, 21);
            return (int) vBase.invoke(o) + "/" + (int) vSub.invoke(o);
        });
        row("set-b-long", () -> {
            Object o = new Base();
            bSet.invoke(o, 9L);
            return (long) bGet.invoke(o);
        });
        row("set-c-ref", () -> {
            Object o = new Base();
            cSet.invoke(o, "z");
            return (String) cGet.invoke(o);
        });
        row("set-d-private", () -> {
            Object o = new Base();
            dSet.invoke(o, 5.5);
            return (double) dGet.invoke(o);
        });
        row("set-e-boolean", () -> {
            Object o = new Deep();
            eSet.invoke(o, false);
            return (boolean) eGet.invoke(o);
        });
        MethodHandle vSubObj = vSub.asType(MethodType.methodType(Object.class, Object.class));
        row("astype-v-sub", () -> vSubObj.invoke(deep));
        MethodHandle vBaseObj = vBase.asType(MethodType.methodType(Object.class, Object.class))
                .asType(MethodType.methodType(Object.class, Sub.class));
        row("astype-astype-v-base", () -> vBaseObj.invoke(sub));
        MethodHandle vSubSetObj = vSubSet.asType(MethodType.methodType(void.class, Object.class, Object.class));
        row("astype-setter-v-sub", () -> {
            Object o = new Sub();
            vSubSetObj.invoke(o, (Object) 33);
            return (int) vBase.invoke(o) + "/" + (int) vSub.invoke(o);
        });
        row("bindTo-v-sub", () -> (int) vSub.bindTo(deep).invoke());
        row("bindTo-setter-a", () -> {
            Object o = new Deep();
            aSet.bindTo(o).invoke(44);
            return (int) aBase.invoke(o);
        });
        row("static-s", () -> (int) sGet.invoke());
        row("static-set-s", () -> {
            sSet.invoke(8);
            return (int) sGet.invoke();
        });

        // The link: made for `Sub`, relinked for `Deep`, alternating (a
        // handful of relinks, then the door).
        MethodHandle exact = vSub.asType(MethodType.methodType(int.class, Object.class));
        MethodHandle exactSet = vSubSet.asType(MethodType.methodType(void.class, Object.class, int.class));
        Object subX = new Sub();
        Object deepX = new Deep();
        exactSet.invokeExact(deepX, 30);
        long n = 0;
        for (int i = 0; i < 1000; i++) {
            n += (int) exact.invokeExact(subX);
            n += (int) exact.invokeExact(deepX);
        }
        System.out.println("warm: " + n);
        Object subY = new Sub();
        Object deepY = new Deep();
        for (int i = 0; i < 1000; i++) {
            exactSet.invokeExact(subY, 40);
            exactSet.invokeExact(deepY, 41);
        }
        System.out.println("warm-setter: " + (int) exact.invokeExact(deepX) + "/"
                + (int) exact.invokeExact(subY) + "/" + (int) exact.invokeExact(deepY));
        row("after-v-sub-on-sub", () -> (int) vSub.invoke(subX));
        row("after-v-sub-on-deep", () -> (int) vSub.invoke(deepX));
        row("after-exact-string", () -> (int) exact.invokeExact((Object) "s"));
        row("after-exact-base", () -> (int) exact.invokeExact(base));
        row("after-invoke-string", () -> (int) vSub.invoke((Object) "s"));
        row("after-invoke-null", () -> (int) vSub.invoke((Object) null));
        row("after-setter-null", () -> {
            exactSet.invokeExact((Object) null, 1);
            return "stored";
        });
    }
}
