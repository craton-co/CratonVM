// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 69, lane L4 -- `CRATONVM_MH_LINKING_CALL_SERVED=1`
// (default off; proposal
// `i68-L4-proposal-a-linking-call-is-served-by-its-link-20261101.md`): the
// `invokeExact` / `invoke` call that WRITES a field getter / setter link is
// served by that link instead of the door. No row may move under the switch.
//
// Every row's handle is FRESH (a new `findGetter` / `findSetter` /
// `findStatic*`, or a new `bindTo` copy), so the row's first call is the
// linking call: its value, its store, and what the calls after it see -- a
// null receiver, a receiver of the owner's subclass or of a subclass that
// shadows the field, a call site of another type (`WrongMethodTypeException`),
// a widened return, a static of a class first initialized by the call, a
// reference setter's value of another class / `null` / an `Object`-typed
// value of the wrong class.
//
// Not a row: an `Object`-typed `invoke` receiver of an unrelated class (a
// `String`) -- its site is not the declared type, so the switch never reaches
// it, and `--compatible` has the pre-existing
// `i65-orch-compatible-warmed-field-getter-accepts-a-string-receiver` defect.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// first-get-exact: 1
// first-get-invoke: 1
// first-get-sub: 6
// first-get-shadow: 1
// first-get-shadow-invoke: 1
// first-get-relink: 56
// first-get-null: java.lang.NullPointerException
// first-then-null: java.lang.NullPointerException
// first-then-null-invoke: java.lang.NullPointerException
// first-get-wmte: java.lang.invoke.WrongMethodTypeException: handle's method type (Base)int but found (Base)long
// first-get-wmte-sub: java.lang.invoke.WrongMethodTypeException: handle's method type (Base)int but found (Sub)int
// first-get-after-wmte: 1
// first-get-long-invoke: 1
// first-get-object-return: 1
// first-set-exact: 7
// first-set-invoke: 8
// first-set-sub: 9
// first-set-short-invoke: 10
// first-set-boxed-invoke: 11
// first-set-null-receiver: java.lang.NullPointerException
// first-static-exact: 4
// first-static-invoke: 4
// first-static-set-exact: 5
// first-static-set-invoke: 6
// Lazy.<clinit>
// first-static-lazy: 11
// Lazy2.<clinit>
// first-static-lazy-set-invoke: 12
// first-bound-get-exact: 24
// first-bound-get-invoke: 24
// first-bound-set-exact: 7,8,9
// first-bound-set-invoke: 10,11,12
// first-bound-wmte: java.lang.invoke.WrongMethodTypeException: handle's method type ()int but found ()long
// first-ref-set-sb: sb
// first-ref-set-null: null
// first-ref-set-then-other: x,sb2
// first-ref-set-object-invoke: java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.String
// first-ref-set-string-then-null: null
// two-class-get: 1050
// two-class-three: 1089
// two-class-set: 98,99
// two-class-invoke: 985
// two-class-string: java.lang.ClassCastException: Cannot cast java.lang.String to L4W69LinkingCallServed$Base
// two-class-null: java.lang.NullPointerException
// HOTSPOT_EXPECTED_END
//
// The `two-class-*` rows are for `CRATONVM_MH_BINDTO_FAST_MINT_TWO_CLASSES=1`
// (default off; proposal
// `i68-L4-proposal-fast-mint-line-keeps-a-second-receiver-class-20261101.md`):
// one source bound alternately to receivers of two (then three) classes, so
// its fast-mint line holds two admitted classes; a `String` and a `null` bound
// after that must still take the slow path's cast / the door's NPE.
//
// Switches: run `--compatible` and `--jdk-only`, each with the switches off
// (default), with `CRATONVM_MH_LINKING_CALL_SERVED=1`, with
// `CRATONVM_MH_LINKING_CALL_SERVED=1 CRATONVM_MH_INVOKE_LINKS_FIELD=0`, and with
// `CRATONVM_MH_BINDTO_FAST_MINT_TWO_CLASSES=1` alone and together with
// `CRATONVM_MH_BINDTO_PRELINK_FIELD=1 CRATONVM_MH_BINDTO_FAST_MINT_PRELINK=1`.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L4W69LinkingCallServed`.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;

public class L4W69LinkingCallServed {
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
        public static int L;

        static {
            System.out.println("Lazy.<clinit>");
            L = 11;
        }
    }

    public static class Lazy2 {
        public static int L;

        static {
            System.out.println("Lazy2.<clinit>");
            L = 11;
        }
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

    static final MethodHandles.Lookup LK = MethodHandles.lookup();

    static MethodHandle getter() throws Throwable {
        return LK.findGetter(Base.class, "v", int.class);
    }

    static MethodHandle setter() throws Throwable {
        return LK.findSetter(Base.class, "v", int.class);
    }

    public static void main(String[] args) throws Throwable {
        Base box = new Base();
        Sub sub = new Sub();
        sub.v = 6;
        Shadow sh = new Shadow();

        // ---- unbound instance getters: the first call links ----
        row("first-get-exact", () -> (int) getter().invokeExact(box));
        row("first-get-invoke", () -> (int) getter().invoke(box));
        row("first-get-sub", () -> (int) getter().invokeExact((Base) sub));
        row("first-get-shadow", () -> (int) getter().invokeExact((Base) sh));
        row("first-get-shadow-invoke", () -> (int) getter().invoke((Base) sh));
        row("first-get-relink", () -> {
            MethodHandle g = getter();
            long s = 0;
            for (int i = 0; i < 21; i++) {
                Base r = i % 3 == 0 ? box : (i % 3 == 1 ? sub : sh);
                s += (int) g.invokeExact(r);
            }
            return s;
        });
        row("first-get-null", () -> (int) getter().invokeExact((Base) null));
        row("first-then-null", () -> {
            MethodHandle g = getter();
            int first = (int) g.invokeExact(box);
            return first + (int) g.invokeExact((Base) null);
        });
        row("first-then-null-invoke", () -> {
            MethodHandle g = getter();
            int first = (int) g.invoke(box);
            return first + (int) g.invoke((Base) null);
        });
        MethodHandle wmte = getter();
        row("first-get-wmte", () -> (long) wmte.invokeExact(box));
        row("first-get-wmte-sub", () -> (int) wmte.invokeExact(sub));
        row("first-get-after-wmte", () -> (int) wmte.invokeExact(box));
        row("first-get-long-invoke", () -> (long) getter().invoke(box));
        row("first-get-object-return", () -> (Object) getter().invoke(box));

        // ---- unbound instance setters ----
        row("first-set-exact", () -> {
            setter().invokeExact(box, 7);
            return box.v;
        });
        row("first-set-invoke", () -> {
            setter().invoke(box, 8);
            return box.v;
        });
        row("first-set-sub", () -> {
            setter().invokeExact((Base) sub, 9);
            return sub.v;
        });
        row("first-set-short-invoke", () -> {
            setter().invoke(box, (short) 10);
            return box.v;
        });
        row("first-set-boxed-invoke", () -> {
            setter().invoke(box, Integer.valueOf(11));
            return box.v;
        });
        row("first-set-null-receiver", () -> {
            setter().invokeExact((Base) null, 1);
            return box.v;
        });
        box.v = 1;
        sub.v = 6;

        // ---- statics, initialized and not ----
        row("first-static-exact", () -> (int) LK.findStaticGetter(Base.class, "S", int.class).invokeExact());
        row("first-static-invoke", () -> (int) LK.findStaticGetter(Base.class, "S", int.class).invoke());
        row("first-static-set-exact", () -> {
            LK.findStaticSetter(Base.class, "S", int.class).invokeExact(5);
            return Base.S;
        });
        row("first-static-set-invoke", () -> {
            LK.findStaticSetter(Base.class, "S", int.class).invoke(6);
            return Base.S;
        });
        Base.S = 4;
        MethodHandle lazy = LK.findStaticGetter(Lazy.class, "L", int.class);
        row("first-static-lazy", () -> (int) lazy.invokeExact());
        MethodHandle lazy2 = LK.findStaticSetter(Lazy2.class, "L", int.class);
        row("first-static-lazy-set-invoke", () -> {
            lazy2.invoke(12);
            return Lazy2.L;
        });

        // ---- bound copies: each copy's first call links ----
        MethodHandle get = getter();
        MethodHandle set = setter();
        row("first-bound-get-exact", () -> {
            long s = 0;
            for (int i = 0; i < 9; i++) {
                Base r = i % 3 == 0 ? box : (i % 3 == 1 ? sub : sh);
                s += (int) get.bindTo(r).invokeExact();
            }
            return s;
        });
        row("first-bound-get-invoke", () -> {
            long s = 0;
            for (int i = 0; i < 9; i++) {
                Base r = i % 3 == 0 ? box : (i % 3 == 1 ? sub : sh);
                s += (int) get.bindTo(r).invoke();
            }
            return s;
        });
        row("first-bound-set-exact", () -> {
            set.bindTo(box).invokeExact(7);
            set.bindTo(sub).invokeExact(8);
            set.bindTo(sh).invokeExact(9);
            return box.v + "," + sub.v + "," + ((Base) sh).v;
        });
        row("first-bound-set-invoke", () -> {
            set.bindTo(box).invoke(10);
            set.bindTo(sub).invoke(11);
            set.bindTo(sh).invoke(12);
            return box.v + "," + sub.v + "," + ((Base) sh).v;
        });
        row("first-bound-wmte", () -> (long) get.bindTo(box).invokeExact());

        // ---- reference setters ----
        row("first-ref-set-sb", () -> {
            LK.findSetter(Base.class, "cs", CharSequence.class)
                .invokeExact(box, (CharSequence) new StringBuilder("sb"));
            return box.cs;
        });
        row("first-ref-set-null", () -> {
            LK.findSetter(Base.class, "cs", CharSequence.class).invokeExact(box, (CharSequence) null);
            return box.cs;
        });
        row("first-ref-set-then-other", () -> {
            MethodHandle h = LK.findSetter(Base.class, "cs", CharSequence.class);
            h.invokeExact(box, (CharSequence) "x");
            String first = String.valueOf(box.cs);
            h.invokeExact(box, (CharSequence) new StringBuilder("sb2"));
            return first + "," + box.cs;
        });
        row("first-ref-set-object-invoke", () -> {
            LK.findSetter(Base.class, "s", String.class).invoke(box, (Object) Integer.valueOf(1));
            return box.s;
        });
        row("first-ref-set-string-then-null", () -> {
            MethodHandle h = LK.findSetter(Base.class, "s", String.class);
            h.invokeExact(box, "y");
            h.invokeExact(box, (String) null);
            return box.s;
        });

        // ---- one source, bound to receivers of two / three classes ----
        // Here box.v = 10, sub.v = 11, ((Base) sh).v = 12.
        MethodHandle get2 = getter();
        MethodHandle set2 = setter();
        row("two-class-get", () -> {
            long s = 0;
            for (int i = 0; i < 100; i++) {
                s += (int) get2.bindTo(i % 2 == 0 ? box : sub).invokeExact();
            }
            return s;
        });
        row("two-class-three", () -> {
            long s = 0;
            for (int i = 0; i < 99; i++) {
                Base r = i % 3 == 0 ? box : (i % 3 == 1 ? sub : sh);
                s += (int) get2.bindTo(r).invokeExact();
            }
            return s;
        });
        row("two-class-set", () -> {
            for (int i = 0; i < 100; i++) {
                set2.bindTo(i % 2 == 0 ? box : sub).invokeExact(i);
            }
            return box.v + "," + sub.v;
        });
        row("two-class-invoke", () -> {
            long s = 0;
            for (int i = 0; i < 10; i++) {
                s += (int) get2.bindTo(i % 2 == 0 ? box : sub).invoke();
            }
            return s;
        });
        row("two-class-string", () -> (int) get2.bindTo("str").invokeExact());
        row("two-class-null", () -> (int) get2.bindTo(null).invokeExact());
    }
}
