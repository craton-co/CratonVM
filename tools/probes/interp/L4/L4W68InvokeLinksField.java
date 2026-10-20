// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 68, lane L4 -- two opt-in switches, no row may
// move under either:
//
// `CRATONVM_MH_INVOKE_LINKS_FIELD=1` (proposal
// `i67-L4-proposal-invoke-links-a-field-accessor-at-the-declared-type-20261031.md`):
// an `invoke` at EXACTLY a field accessor's declared type links it, so a
// handle only ever called through `invoke` is served by the link
// (`CRATONVM_MH_FIELD_LINK_SERVES_INVOKE`, default on). Every handle in the
// `invoke-only-*` rows is fresh and never sees an `invokeExact`: the
// declared-type loop (which links it under the switch) is followed by the
// rows the link must leave to the door -- a null receiver, a widened return,
// receivers of the owner's subclasses at the declared type, a setter's
// `short` / `Integer` value, a reference setter's value of another class and
// `null`, an `Object`-typed value of the wrong class -- plus bound handles,
// static handles and a static handle of a class first initialized by the
// `invoke` itself.
//
// `CRATONVM_MH_BINDTO_FAST_MINT_PRELINK=1` with
// `CRATONVM_MH_BINDTO_PRELINK_FIELD=1` (proposal
// `i67-L4-proposal-fast-mint-line-carries-the-pre-link-20261031.md`): a
// `bindTo` fast-mint hit writes the link the slow path's pre-link derived.
// The `bind-*` rows bind many receivers -- two objects of one class
// alternately (hits: the replayed link must read and write the receiver
// bound THIS time), and receivers of `Base` / `Sub` / `Shadow` alternately
// (the remembered class does not match: re-derived).
//
// The pre-existing `i65-orch-compatible-warmed-field-getter-accepts-a-string-receiver`
// defect (an `Object`-typed `invoke` site with a `String` receiver) is NOT a
// row here: its site is not the declared type, so neither switch reaches it;
// `L4W65FieldLinkSubclasses`'s `string` row must keep its current answer.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// invoke-only-declared: 3000
// invoke-only-mixed: 300
// invoke-only-null: java.lang.NullPointerException
// invoke-only-long-return: 1
// invoke-only-object-return: 1
// invoke-only-object-site: 100
// invoke-only-setter: 2999
// invoke-only-setter-short: 7
// invoke-only-setter-boxed: 8
// invoke-only-ref-setter: y
// invoke-only-ref-setter-object: java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.String
// invoke-only-cs-builder: sb
// invoke-only-cs-null: null
// invoke-only-bound: 15000
// invoke-only-bound-setter: 2999
// invoke-only-static: 12000
// invoke-only-static-setter: 9
// invoke-only-lazy-static: 11000
// invoke-only-getter-type: (Base)int
// bind-same-class-get: 350
// bind-same-class-set: 98,99
// bind-same-class-invoke: 350
// bind-mixed-get: 900
// bind-mixed-set: 298,299
// bind-shadow-get: 100
// bind-ref-setter: sb,null
// HOTSPOT_EXPECTED_END
//
// Switches: run `--compatible` and `--jdk-only`, each with the switches off
// (default), with `CRATONVM_MH_INVOKE_LINKS_FIELD=1`, and with
// `CRATONVM_MH_BINDTO_PRELINK_FIELD=1 CRATONVM_MH_BINDTO_FAST_MINT_PRELINK=1`.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L4W68InvokeLinksField`.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;

public class L4W68InvokeLinksField {
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

    public static class Lazy {
        public static int L = 11;
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

        // ---- `invoke` only: no handle below ever sees `invokeExact` ----
        MethodHandle getV = lk.findGetter(Base.class, "v", int.class);
        long n = 0;
        for (int i = 0; i < 3000; i++) {
            n += (int) getV.invoke(box);
        }
        System.out.println("invoke-only-declared: " + n);
        // Receivers of the owner's subclasses at the declared type: `Base.v`.
        long mixed = 0;
        for (int i = 0; i < 300; i++) {
            Base r = i % 3 == 0 ? box : (i % 3 == 1 ? sub : sh);
            mixed += (int) getV.invoke(r);
        }
        System.out.println("invoke-only-mixed: " + mixed);
        row("invoke-only-null", () -> (int) getV.invoke((Base) null));
        row("invoke-only-long-return", () -> (long) getV.invoke(box));
        row("invoke-only-object-return", () -> (Object) getV.invoke(box));
        MethodHandle getObj = lk.findGetter(Base.class, "v", int.class);
        row("invoke-only-object-site", () -> {
            long s = 0;
            for (int i = 0; i < 100; i++) {
                s += (int) getObj.invoke((Object) box);
            }
            return s;
        });

        MethodHandle setV = lk.findSetter(Base.class, "v", int.class);
        for (int i = 0; i < 3000; i++) {
            setV.invoke(box, i);
        }
        row("invoke-only-setter", () -> box.v);
        row("invoke-only-setter-short", () -> {
            setV.invoke(box, (short) 7);
            return box.v;
        });
        row("invoke-only-setter-boxed", () -> {
            setV.invoke(box, Integer.valueOf(8));
            return box.v;
        });

        MethodHandle setS = lk.findSetter(Base.class, "s", String.class);
        for (int i = 0; i < 100; i++) {
            setS.invoke(box, "a");
        }
        row("invoke-only-ref-setter", () -> {
            setS.invoke(box, "y");
            return box.s;
        });
        row("invoke-only-ref-setter-object", () -> {
            setS.invoke(box, (Object) Integer.valueOf(1));
            return box.s;
        });
        MethodHandle setCs = lk.findSetter(Base.class, "cs", CharSequence.class);
        for (int i = 0; i < 100; i++) {
            setCs.invoke(box, (CharSequence) "x");
        }
        row("invoke-only-cs-builder", () -> {
            setCs.invoke(box, (CharSequence) new StringBuilder("sb"));
            return box.cs;
        });
        row("invoke-only-cs-null", () -> {
            setCs.invoke(box, (CharSequence) null);
            return box.cs;
        });

        MethodHandle getB = lk.findGetter(Base.class, "v", int.class);
        MethodHandle bv = getB.bindTo(box);
        box.v = 5;
        long b = 0;
        for (int i = 0; i < 3000; i++) {
            b += (int) bv.invoke();
        }
        System.out.println("invoke-only-bound: " + b);
        MethodHandle setB = lk.findSetter(Base.class, "v", int.class);
        MethodHandle bset = setB.bindTo(box);
        for (int i = 0; i < 3000; i++) {
            bset.invoke(i);
        }
        row("invoke-only-bound-setter", () -> box.v);

        MethodHandle sget = lk.findStaticGetter(Base.class, "S", int.class);
        long st = 0;
        for (int i = 0; i < 3000; i++) {
            st += (int) sget.invoke();
        }
        System.out.println("invoke-only-static: " + st);
        MethodHandle sset = lk.findStaticSetter(Base.class, "S", int.class);
        row("invoke-only-static-setter", () -> {
            for (int i = 0; i < 100; i++) {
                sset.invoke(i);
            }
            sset.invoke(9);
            return Base.S;
        });
        MethodHandle lazy = lk.findStaticGetter(Lazy.class, "L", int.class);
        row("invoke-only-lazy-static", () -> {
            long s = 0;
            for (int i = 0; i < 1000; i++) {
                s += (int) lazy.invoke();
            }
            return s;
        });
        row("invoke-only-getter-type", () -> getV.type());

        // ---- `bindTo` many times: the fast-mint line and its pre-link ----
        MethodHandle get = lk.findGetter(Base.class, "v", int.class);
        MethodHandle set = lk.findSetter(Base.class, "v", int.class);
        Base a1 = new Base();
        Base a2 = new Base();
        a1.v = 3;
        a2.v = 4;
        row("bind-same-class-get", () -> {
            long s = 0;
            for (int i = 0; i < 100; i++) {
                s += (int) get.bindTo(i % 2 == 0 ? a1 : a2).invokeExact();
            }
            return s;
        });
        row("bind-same-class-set", () -> {
            for (int i = 0; i < 100; i++) {
                set.bindTo(i % 2 == 0 ? a1 : a2).invokeExact(i);
            }
            return a1.v + "," + a2.v;
        });
        a1.v = 3;
        a2.v = 4;
        row("bind-same-class-invoke", () -> {
            long s = 0;
            for (int i = 0; i < 100; i++) {
                s += (int) get.bindTo(i % 2 == 0 ? a1 : a2).invoke();
            }
            return s;
        });
        box.v = 5;
        row("bind-mixed-get", () -> {
            long s = 0;
            for (int i = 0; i < 300; i++) {
                Base r = i % 2 == 0 ? box : sub;
                s += (int) get.bindTo(r).invokeExact();
            }
            return s;
        });
        row("bind-mixed-set", () -> {
            for (int i = 0; i < 300; i++) {
                Base r = i % 2 == 0 ? box : sub;
                set.bindTo(r).invokeExact(i);
            }
            return box.v + "," + sub.v;
        });
        row("bind-shadow-get", () -> {
            long s = 0;
            for (int i = 0; i < 100; i++) {
                s += (int) get.bindTo(sh).invokeExact();
            }
            return s;
        });
        MethodHandle setCs2 = lk.findSetter(Base.class, "cs", CharSequence.class);
        row("bind-ref-setter", () -> {
            for (int i = 0; i < 100; i++) {
                setCs2.bindTo(a1).invokeExact((CharSequence) "x");
            }
            setCs2.bindTo(a1).invokeExact((CharSequence) new StringBuilder("sb"));
            String first = String.valueOf(a1.cs);
            setCs2.bindTo(a1).invokeExact((CharSequence) null);
            return first + "," + a1.cs;
        });
    }
}
