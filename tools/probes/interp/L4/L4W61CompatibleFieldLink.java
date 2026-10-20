// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 61, lane L4 -- proposal
// `interpreter-L4-proposal-link-field-handles-under-compatible-too-FIXED-20261025`: the
// field-accessor link (`CRATONVM_MH_FIELD_LINK`) is made under `--compatible`
// too (`CRATONVM_MH_FIELD_LINK_COMPATIBLE`). A link must never change an
// answer: every row runs a field getter / setter `invokeExact` many times
// (linked after the first call), then the shapes the link leaves to the door
// -- a receiver of another class, a null receiver, a subclass receiver -- on
// the same, now linked, handle.
//
// Positive control: `CRATONVM_DBG_MH_FIELD_LINK=1 cratonvm --compatible`
// prints `[mh-field-link] --compatible ... link` lines (stderr) with the
// switch on, none with `CRATONVM_MH_FIELD_LINK_COMPATIBLE=0`.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint)
// int-getter: 7
// int-getter-loop: 70000
// int-setter-loop: 9999
// ref-getter-loop: 10000
// ref-setter: b
// static-getter-loop: 30000
// static-setter: 2
// subclass-receiver: 7
// other-receiver: java.lang.ClassCastException
// null-receiver: java.lang.NullPointerException
// wrong-site-type: java.lang.invoke.WrongMethodTypeException
// int-getter-again: 9999
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L4W61CompatibleFieldLink`.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;

public class L4W61CompatibleFieldLink {
    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        try {
            System.out.println(name + ": " + r.run());
        } catch (Throwable t) {
            System.out.println(name + ": " + t.getClass().getName());
        }
    }

    static class Box {
        int i = 7;
        String s = "a";
        static long count = 3;
    }

    static class SubBox extends Box {}

    static class Other {
        int i = 5;
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodHandle getI = l.findGetter(Box.class, "i", int.class);
        MethodHandle setI = l.findSetter(Box.class, "i", int.class);
        MethodHandle getS = l.findGetter(Box.class, "s", String.class);
        MethodHandle setS = l.findSetter(Box.class, "s", String.class);
        MethodHandle getCount = l.findStaticGetter(Box.class, "count", long.class);
        MethodHandle setCount = l.findStaticSetter(Box.class, "count", long.class);
        Box box = new Box();
        row("int-getter", () -> (int) getI.invokeExact(box));
        row("int-getter-loop", () -> {
            long sum = 0;
            for (int k = 0; k < 10000; k++) {
                sum += (int) getI.invokeExact(box);
            }
            return sum;
        });
        row("int-setter-loop", () -> {
            for (int k = 0; k < 10000; k++) {
                setI.invokeExact(box, k);
            }
            return box.i;
        });
        row("ref-getter-loop", () -> {
            long n = 0;
            for (int k = 0; k < 10000; k++) {
                n += ((String) getS.invokeExact(box)).length();
            }
            return n;
        });
        row("ref-setter", () -> {
            for (int k = 0; k < 10000; k++) {
                String v = (k & 1) == 0 ? "a" : "b";
                setS.invokeExact(box, v);
            }
            return box.s;
        });
        row("static-getter-loop", () -> {
            long sum = 0;
            for (int k = 0; k < 10000; k++) {
                sum += (long) getCount.invokeExact();
            }
            return sum;
        });
        row("static-setter", () -> {
            for (int k = 0; k < 10000; k++) {
                setCount.invokeExact((long) (k % 13));
            }
            return Box.count;
        });
        row("subclass-receiver", () -> {
            SubBox sub = new SubBox();
            return (int) getI.invokeExact((Box) sub);
        });
        MethodHandle getIObject = getI.asType(getI.type().changeParameterType(0, Object.class));
        for (int k = 0; k < 1000; k++) {
            int ignored = (int) getIObject.invokeExact((Object) box);
        }
        row("other-receiver", () -> (int) getIObject.invokeExact((Object) new Other()));
        row("null-receiver", () -> (int) getI.invokeExact((Box) null));
        row("wrong-site-type", () -> (long) getI.invokeExact(box));
        row("int-getter-again", () -> (int) getI.invokeExact(box));
    }
}
