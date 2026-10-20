// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 67, lane L4 -- the field-accessor link serves
// `invoke` at the declared type too (`CRATONVM_MH_FIELD_LINK_SERVES_INVOKE`,
// opt-in; proposal
// `i66-L4-proposal-field-link-serves-invoke-at-the-declared-type-20261030.md`).
//
// Each handle is linked by `invokeExact` first, then called through `invoke`:
// at exactly its declared type (the link serves it with the switch on), and at
// every other site (an `asType`, which stays on the door): a subclass-typed
// receiver, an `Object`-typed receiver, an `Object` / `long` return, a `null`
// receiver at the declared type (the door's message-less NPE), a setter's
// `short` and `Integer` value, a reference setter at its declared type with a
// value of another class (the link's value check) and `null`, an `Object`
// value of the wrong class (`ClassCastException`), bound getters / setters,
// and static getters / setters.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// invoke-declared: 3000
// invoke-sub-as-base: 1
// invoke-shadow-as-base: 1
// invoke-sub-site: 1
// invoke-object-site: 1
// invoke-object-return: 1
// invoke-long-return: 1
// invoke-null: java.lang.NullPointerException
// setter-invoke: 2999
// setter-invoke-short: 7
// setter-invoke-boxed: 8
// ref-setter-invoke: y
// ref-setter-invoke-object: java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.String
// cs-invoke-builder: sb
// cs-invoke-null: null
// bound-invoke: 15000
// bound-invoke-object: 5
// bound-setter-invoke: 2999
// static-invoke: 12000
// static-setter-invoke: 9
// getter-type: (Base)int
// HOTSPOT_EXPECTED_END
//
// Switches: run `--compatible` and `--jdk-only`, each with the switch off
// (default) and with `CRATONVM_MH_FIELD_LINK_SERVES_INVOKE=1`: no row may
// move. The pre-existing
// `i65-orch-compatible-warmed-field-getter-accepts-a-string-receiver` defect
// (an `Object`-typed `invoke` site with a `String` receiver) is NOT a row
// here: its site is not the declared type, so this switch never reaches it;
// `L4W65FieldLinkSubclasses`'s `string` row must keep its current answer.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L4W67FieldLinkInvoke`.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;

public class L4W67FieldLinkInvoke {
    public static class Base {
        public static int S = 4;
        public int v = 1;
        public String s = "str";
        public CharSequence cs = "init";
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
        MethodHandle setS = lk.findSetter(Base.class, "s", String.class);
        MethodHandle setCs = lk.findSetter(Base.class, "cs", CharSequence.class);
        MethodHandle sget = lk.findStaticGetter(Base.class, "S", int.class);
        MethodHandle sset = lk.findStaticSetter(Base.class, "S", int.class);

        long warm = 0;
        for (int i = 0; i < 100; i++) {
            warm += (int) getV.invokeExact(box);
        }
        long n = 0;
        for (int i = 0; i < 3000; i++) {
            n += (int) getV.invoke(box);
        }
        System.out.println("invoke-declared: " + n);
        row("invoke-sub-as-base", () -> (int) getV.invoke((Base) sub));
        row("invoke-shadow-as-base", () -> (int) getV.invoke((Base) sh));
        row("invoke-sub-site", () -> (int) getV.invoke(sub));
        row("invoke-object-site", () -> (int) getV.invoke((Object) box));
        row("invoke-object-return", () -> (Object) getV.invoke(box));
        row("invoke-long-return", () -> (long) getV.invoke(box));
        row("invoke-null", () -> (int) getV.invoke((Base) null));

        for (int i = 0; i < 100; i++) {
            setV.invokeExact(box, i);
        }
        for (int i = 0; i < 3000; i++) {
            setV.invoke(box, i);
        }
        row("setter-invoke", () -> box.v);
        row("setter-invoke-short", () -> {
            setV.invoke(box, (short) 7);
            return box.v;
        });
        row("setter-invoke-boxed", () -> {
            setV.invoke(box, Integer.valueOf(8));
            return box.v;
        });

        for (int i = 0; i < 100; i++) {
            setS.invokeExact(box, "a");
        }
        row("ref-setter-invoke", () -> {
            setS.invoke(box, "y");
            return box.s;
        });
        row("ref-setter-invoke-object", () -> {
            setS.invoke(box, (Object) Integer.valueOf(1));
            return box.s;
        });
        for (int i = 0; i < 100; i++) {
            setCs.invokeExact(box, (CharSequence) "x");
        }
        row("cs-invoke-builder", () -> {
            setCs.invoke(box, (CharSequence) new StringBuilder("sb"));
            return box.cs;
        });
        row("cs-invoke-null", () -> {
            setCs.invoke(box, (CharSequence) null);
            return box.cs;
        });

        MethodHandle bv = getV.bindTo(box);
        for (int i = 0; i < 100; i++) {
            warm += (int) bv.invokeExact();
        }
        box.v = 5;
        long b = 0;
        for (int i = 0; i < 3000; i++) {
            b += (int) bv.invoke();
        }
        System.out.println("bound-invoke: " + b);
        row("bound-invoke-object", () -> (Object) bv.invoke());
        MethodHandle bset = setV.bindTo(box);
        for (int i = 0; i < 100; i++) {
            bset.invokeExact(i);
        }
        for (int i = 0; i < 3000; i++) {
            bset.invoke(i);
        }
        row("bound-setter-invoke", () -> box.v);

        for (int i = 0; i < 100; i++) {
            warm += (int) sget.invokeExact();
        }
        long st = 0;
        for (int i = 0; i < 3000; i++) {
            st += (int) sget.invoke();
        }
        System.out.println("static-invoke: " + st);
        row("static-setter-invoke", () -> {
            sset.invokeExact(6);
            sset.invoke(9);
            return Base.S;
        });
        row("getter-type", () -> getV.type());
        if (warm < 0) {
            System.out.println("unreachable");
        }
    }
}
