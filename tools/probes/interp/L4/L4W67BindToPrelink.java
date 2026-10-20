// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 67, lane L4 -- `bindTo` pre-links the bound copy
// of a field handle (`CRATONVM_MH_BINDTO_PRELINK_FIELD`, opt-in; proposal
// `i66-L4-proposal-bindto-pre-links-a-bound-field-copy-20261030.md`).
//
// Every row is a FRESH bound copy's first call (or a copy's later call), the
// shape the switch changes: the first `invokeExact` of a `getter.bindTo(x)`
// is served by a link `bindTo` wrote. No row may move: subclass and
// field-SHADOWING receivers (the base slot), a ring of receiver classes, a
// fresh setter per call, `long` fields, a call site that is not the declared
// type on a fresh copy (`WrongMethodTypeException`), `invoke` on a fresh copy
// at another type, reference setters whose value is checked (left to their
// first call), an `Object` setter (pre-linked), a final field, an `asType`
// stamp's bound copy (a cast chain, never linked) with a `String`, `bindTo`
// of a `String`, a write behind a copy's back, and the copy's `type()` and
// `invokeWithArguments`.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// fresh-getter: 3000
// fresh-sub: 1
// fresh-shadow: 1
// fresh-ring: 3000
// fresh-setter: 2999
// fresh-setter-shadow: 2 42
// fresh-wide: 20000
// fresh-wrong-site-int: java.lang.invoke.WrongMethodTypeException: handle's method type ()int but found ()long
// fresh-wrong-site-ref: java.lang.invoke.WrongMethodTypeException: handle's method type ()String but found ()Object
// fresh-invoke-object: str
// fresh-invoke-long: 2999
// fresh-ref-setter: y
// fresh-cs-builder: sb
// fresh-cs-null: null
// fresh-obj-setter: 5
// fresh-ref-setter-invoke-object: java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.String
// fresh-final: 7
// stamp-bound: 1
// stamp-string: java.lang.ClassCastException: Cannot cast java.lang.String to L4W67BindToPrelink$Base
// bind-string: java.lang.ClassCastException: Cannot cast java.lang.String to L4W67BindToPrelink$Base
// after-write: 5
// bound-type: ()int
// bound-iwa: 5
// HOTSPOT_EXPECTED_END
//
// Switches: run `--compatible` and `--jdk-only`, each with the switch off
// (default) and with `CRATONVM_MH_BINDTO_PRELINK_FIELD=1`, and once with it on
// plus `CRATONVM_MH_FIELD_LINK_BOUND=0` (the pre-link needs that link): no row
// may move. Positive control: `CRATONVM_DBG_MH_FIELD_LINK=1 --compatible`
// prints one `[mh-field-link] --compatible instance link ...` line per fresh
// copy with the switch on as well as off -- written by `bindTo` with it on,
// by the first `invokeExact` without it -- except for the `String` /
// `CharSequence` setters' copies, which link on their first call either way.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L4W67BindToPrelink`.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W67BindToPrelink {
    public static class Base {
        public int v = 1;
        public long w = 10L;
        public String s = "str";
        public CharSequence cs = "init";
        public Object o = "obj";
        public final int f = 7;
    }

    public static class Sub extends Base {
    }

    public static class Shadow extends Base {
        public int v = 2;
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
        if (out.startsWith("MethodHandle(")) {
            out = "MethodHandle(...)";
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lk = MethodHandles.lookup();
        Base box = new Base();
        Sub sub = new Sub();
        Shadow sh = new Shadow();
        MethodHandle getV = lk.findGetter(Base.class, "v", int.class);
        MethodHandle setV = lk.findSetter(Base.class, "v", int.class);
        MethodHandle getW = lk.findGetter(Base.class, "w", long.class);
        MethodHandle getS = lk.findGetter(Base.class, "s", String.class);
        MethodHandle setS = lk.findSetter(Base.class, "s", String.class);
        MethodHandle setCs = lk.findSetter(Base.class, "cs", CharSequence.class);
        MethodHandle setO = lk.findSetter(Base.class, "o", Object.class);
        MethodHandle getF = lk.findGetter(Base.class, "f", int.class);

        long n = 0;
        for (int i = 0; i < 3000; i++) {
            n += (int) getV.bindTo(box).invokeExact();
        }
        System.out.println("fresh-getter: " + n);
        row("fresh-sub", () -> (int) getV.bindTo(sub).invokeExact());
        row("fresh-shadow", () -> (int) getV.bindTo(sh).invokeExact());
        Base[] ring = {box, sub, sh};
        long mix = 0;
        for (int i = 0; i < 3000; i++) {
            mix += (int) getV.bindTo(ring[i % 3]).invokeExact();
        }
        System.out.println("fresh-ring: " + mix);

        for (int i = 0; i < 3000; i++) {
            setV.bindTo(box).invokeExact(i);
        }
        row("fresh-setter", () -> box.v);
        row("fresh-setter-shadow", () -> {
            setV.bindTo(sh).invokeExact(42);
            return sh.v + " " + ((Base) sh).v;
        });

        long lw = 0;
        for (int i = 0; i < 2000; i++) {
            lw += (long) getW.bindTo(box).invokeExact();
        }
        System.out.println("fresh-wide: " + lw);

        row("fresh-wrong-site-int", () -> {
            long x = (long) getV.bindTo(box).invokeExact();
            return x;
        });
        row("fresh-wrong-site-ref", () -> {
            Object x = (Object) getS.bindTo(box).invokeExact();
            return x;
        });
        row("fresh-invoke-object", () -> (Object) getS.bindTo(box).invoke());
        row("fresh-invoke-long", () -> (long) getV.bindTo(box).invoke());

        row("fresh-ref-setter", () -> {
            setS.bindTo(box).invokeExact("y");
            return box.s;
        });
        row("fresh-cs-builder", () -> {
            setCs.bindTo(box).invokeExact((CharSequence) new StringBuilder("sb"));
            return box.cs;
        });
        row("fresh-cs-null", () -> {
            setCs.bindTo(box).invokeExact((CharSequence) null);
            return box.cs;
        });
        row("fresh-obj-setter", () -> {
            setO.bindTo(box).invokeExact((Object) Integer.valueOf(5));
            return box.o;
        });
        row("fresh-ref-setter-invoke-object", () -> {
            setS.bindTo(box).invoke((Object) Integer.valueOf(1));
            return box.s;
        });
        row("fresh-final", () -> (int) getF.bindTo(box).invokeExact());

        MethodHandle objV = getV.asType(MethodType.methodType(int.class, Object.class));
        row("stamp-bound", () -> (int) objV.bindTo(sub).invokeExact());
        row("stamp-string", () -> (int) objV.bindTo("x").invokeExact());
        row("bind-string", () -> getV.bindTo("x"));

        MethodHandle bv = getV.bindTo(box);
        box.v = 5;
        row("after-write", () -> (int) bv.invokeExact());
        row("bound-type", () -> getV.bindTo(box).type());
        row("bound-iwa", () -> getV.bindTo(box).invokeWithArguments());
    }
}
