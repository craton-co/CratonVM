// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 70, lane L4 -- two default-off switches that
// change how a field-accessor link is DERIVED, never what it answers:
//   CRATONVM_MH_INVOKE_LINK_DECLARED_ONCE=1 (a pure refactor; proposal
//     `i69-L4-proposal-invoke-link-renders-the-declared-type-once-20261102`)
//   CRATONVM_MH_FIELD_LINK_DERIVATION_MEMO=1 (proposal
//     `i69-L4-proposal-field-link-derivation-is-memoised-per-source-shape-20261102`)
// No row may move under either switch, in either mode.
//
// The memo rows repeat ONE shape many times on fresh handles (bound copies of
// one source; fresh finder handles) so a line is filled and then hit, and
// then change exactly one thing the memo must re-judge: the receiver's class
// (an owner subclass, a subclass that shadows the field), a null receiver, a
// receiver of an unrelated class, a checked setter value of another class /
// `null` / the wrong class through an `Object` site, the call site's type
// (`WrongMethodTypeException`), the arity, another field of the same
// descriptor (the same memo line, another key), another owner's field of the
// same name. The `invoke` rows at the declared type are the refactor's.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// bound-get-exact: 80
// bound-get-invoke: 80
// bound-get-same-class: 50
// bound-get-null: java.lang.NullPointerException
// bound-get-string: java.lang.ClassCastException: Cannot cast java.lang.String to L4W70LinkDerivation$Base
// bound-get-wmte-long: java.lang.invoke.WrongMethodTypeException: handle's method type ()int but found ()long
// bound-get-wmte-object: java.lang.invoke.WrongMethodTypeException: handle's method type ()int but found ()Object
// bound-get-object-invoke: 1
// bound-get-long-invoke: 1
// bound-get-arity-invoke: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle()int to (int)int
// bound-get-after: 8
// bound-set-exact: 27,28,29
// bound-set-invoke: 127,128,129
// bound-set-short-invoke: 7
// bound-set-boxed-invoke: 8
// bound-ref-set-classes: sb0;st1;null;sb3;st4;null;sb6;st7;null;sb9;st10;null;
// bound-ref-set-wrong-object: java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.CharSequence
// bound-ref-set-string-field: x9
// bound-ref-set-string-wrong: java.lang.ClassCastException: Cannot cast java.lang.StringBuilder to java.lang.String
// bound-two-fields: 100
// bound-shadow-pair: 15
// find-get-exact: 32
// find-get-invoke: 32
// find-get-null: java.lang.NullPointerException
// find-get-wmte-sub: java.lang.invoke.WrongMethodTypeException: handle's method type (Base)int but found (Sub)int
// find-get-long-invoke: 1
// find-get-object-invoke: 1
// find-set-exact: 9,10,11
// find-set-invoke: 109,110,111
// find-set-null-receiver: java.lang.NullPointerException
// find-ref-set-classes: sb0;st1;null;sb3;st4;null;
// static-get: 40
// bound-set-arity-invoke: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(int)void to (int,int)void
// find-get-arity-invoke: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(Base)int to (Base,int)int
// find-set-arity-invoke: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(Base,int)void to (Base)void
// static-get-arity-invoke: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle()int to (int)int
// arity-left: 1
// HOTSPOT_EXPECTED_END
//
// Wave 79 (lane L4; page
// `i70-orch-compatible-bound-getter-invoke-ignores-an-extra-argument`): the
// last five rows, a field accessor `invoke`d at another arity (bound or not,
// static or instance), are `asType`'s refusal; `arity-left` checks nothing was
// written.
//
// Switches: run `--compatible` and `--jdk-only`, each with the switches off
// (default), with each one alone, with both, and with both plus
// `CRATONVM_MH_LINKING_CALL_SERVED=1` (wave 69: the linking call is then
// served by the link the memo wrote). Optionally also with
// `CRATONVM_MH_RETURN_PLAN=0` (the refactor's render path).
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L4W70LinkDerivation`.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;

public class L4W70LinkDerivation {
    public static class Base {
        public static int S = 4;
        public int v = 1;
        public int w = 9;
        public String s = "s";
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
        System.out.println(name + ": " + out);
    }

    static final MethodHandles.Lookup LK = MethodHandles.lookup();

    static Base box = new Base();
    static Sub sub = new Sub();
    static Shadow sh = new Shadow();

    /** box.v = 1, sub.v = 6, sh's Base.v = 1 (its own v = 2); the strings back to their start. */
    static void reset() {
        box.v = 1;
        sub.v = 6;
        ((Base) sh).v = 1;
        sh.v = 2;
        box.s = "s";
        box.cs = "init";
        sub.cs = "init";
    }

    static Base rot(int i) {
        return i % 3 == 0 ? box : (i % 3 == 1 ? sub : sh);
    }

    static MethodHandle getter() throws Throwable {
        return LK.findGetter(Base.class, "v", int.class);
    }

    static MethodHandle setter() throws Throwable {
        return LK.findSetter(Base.class, "v", int.class);
    }

    public static void main(String[] args) throws Throwable {
        reset();
        MethodHandle get = getter();
        MethodHandle set = setter();
        MethodHandle getW = LK.findGetter(Base.class, "w", int.class);
        MethodHandle getShV = LK.findGetter(Shadow.class, "v", int.class);
        MethodHandle csSet = LK.findSetter(Base.class, "cs", CharSequence.class);
        MethodHandle sSet = LK.findSetter(Base.class, "s", String.class);

        // ---- bound copies of one source: the shape repeats ----
        row("bound-get-exact", () -> {
            long s = 0;
            for (int i = 0; i < 30; i++) s += (int) get.bindTo(rot(i)).invokeExact();
            return s;
        });
        row("bound-get-invoke", () -> {
            long s = 0;
            for (int i = 0; i < 30; i++) s += (int) get.bindTo(rot(i)).invoke();
            return s;
        });
        row("bound-get-same-class", () -> {
            long s = 0;
            for (int i = 0; i < 50; i++) s += (int) get.bindTo(box).invokeExact();
            return s;
        });
        row("bound-get-null", () -> (int) get.bindTo(null).invokeExact());
        row("bound-get-string", () -> (int) get.bindTo("str").invokeExact());
        row("bound-get-wmte-long", () -> (long) get.bindTo(box).invokeExact());
        row("bound-get-wmte-object", () -> (Object) get.bindTo(box).invokeExact());
        row("bound-get-object-invoke", () -> (Object) get.bindTo(box).invoke());
        row("bound-get-long-invoke", () -> (long) get.bindTo(box).invoke());
        row("bound-get-arity-invoke", () -> (int) get.bindTo(box).invoke(5));
        row("bound-get-after", () -> {
            long s = 0;
            for (int i = 0; i < 3; i++) s += (int) get.bindTo(rot(i)).invokeExact();
            return s;
        });

        row("bound-set-exact", () -> {
            for (int i = 0; i < 30; i++) set.bindTo(rot(i)).invokeExact(i);
            return box.v + "," + sub.v + "," + ((Base) sh).v;
        });
        reset();
        row("bound-set-invoke", () -> {
            for (int i = 0; i < 30; i++) set.bindTo(rot(i)).invoke(i + 100);
            return box.v + "," + sub.v + "," + ((Base) sh).v;
        });
        reset();
        row("bound-set-short-invoke", () -> {
            for (int i = 0; i < 10; i++) set.bindTo(box).invoke((short) 7);
            return box.v;
        });
        row("bound-set-boxed-invoke", () -> {
            for (int i = 0; i < 10; i++) set.bindTo(box).invoke(Integer.valueOf(8));
            return box.v;
        });
        reset();

        // ---- a checked reference setter: the value's class is re-judged ----
        row("bound-ref-set-classes", () -> {
            StringBuilder out = new StringBuilder();
            for (int i = 0; i < 12; i++) {
                Base r = i % 2 == 0 ? box : sub;
                CharSequence v = i % 3 == 0 ? new StringBuilder("sb" + i) : (i % 3 == 1 ? "st" + i : null);
                csSet.bindTo(r).invokeExact(v);
                out.append(r.cs).append(';');
            }
            return out;
        });
        row("bound-ref-set-wrong-object", () -> {
            csSet.bindTo(box).invoke((Object) Integer.valueOf(1));
            return box.cs;
        });
        row("bound-ref-set-string-field", () -> {
            for (int i = 0; i < 10; i++) sSet.bindTo(box).invokeExact("x" + i);
            return box.s;
        });
        row("bound-ref-set-string-wrong", () -> {
            sSet.bindTo(box).invoke((Object) new StringBuilder("q"));
            return box.s;
        });
        reset();

        // ---- another field of the same descriptor; another owner's field ----
        row("bound-two-fields", () -> {
            long s = 0;
            for (int i = 0; i < 20; i++) s += (int) (i % 2 == 0 ? get : getW).bindTo(box).invokeExact();
            return s;
        });
        row("bound-shadow-pair", () -> {
            long s = 0;
            for (int i = 0; i < 10; i++) {
                s += i % 2 == 0 ? (int) get.bindTo(sh).invokeExact() : (int) getShV.bindTo(sh).invokeExact();
            }
            return s;
        });

        // ---- fresh finder handles: the first call links ----
        row("find-get-exact", () -> {
            long s = 0;
            for (int i = 0; i < 12; i++) s += (int) getter().invokeExact(rot(i));
            return s;
        });
        row("find-get-invoke", () -> {
            long s = 0;
            for (int i = 0; i < 12; i++) s += (int) getter().invoke(rot(i));
            return s;
        });
        row("find-get-null", () -> (int) getter().invokeExact((Base) null));
        row("find-get-wmte-sub", () -> (int) getter().invokeExact(sub));
        row("find-get-long-invoke", () -> (long) getter().invoke(box));
        row("find-get-object-invoke", () -> (Object) getter().invoke((Object) box));
        row("find-set-exact", () -> {
            for (int i = 0; i < 12; i++) setter().invokeExact(rot(i), i);
            return box.v + "," + sub.v + "," + ((Base) sh).v;
        });
        reset();
        row("find-set-invoke", () -> {
            for (int i = 0; i < 12; i++) setter().invoke(rot(i), i + 100);
            return box.v + "," + sub.v + "," + ((Base) sh).v;
        });
        reset();
        row("find-set-null-receiver", () -> {
            setter().invokeExact((Base) null, 1);
            return box.v;
        });
        row("find-ref-set-classes", () -> {
            StringBuilder out = new StringBuilder();
            for (int i = 0; i < 6; i++) {
                CharSequence v = i % 3 == 0 ? new StringBuilder("sb" + i) : (i % 3 == 1 ? "st" + i : null);
                LK.findSetter(Base.class, "cs", CharSequence.class).invokeExact(box, v);
                out.append(box.cs).append(';');
            }
            return out;
        });
        reset();

        // ---- a static: never memoised (control) ----
        row("static-get", () -> {
            long s = 0;
            for (int i = 0; i < 10; i++) s += (int) LK.findStaticGetter(Base.class, "S", int.class).invokeExact();
            return s;
        });

        // ---- wave 79: the arity of a field accessor's `invoke` ----
        reset();
        row("bound-set-arity-invoke", () -> {
            setter().bindTo(box).invoke(5, 6);
            return "stored";
        });
        row("find-get-arity-invoke", () -> (int) getter().invoke(box, 5));
        row("find-set-arity-invoke", () -> {
            setter().invoke(box);
            return "stored";
        });
        row("static-get-arity-invoke", () -> (int) LK.findStaticGetter(Base.class, "S", int.class).invoke(5));
        row("arity-left", () -> box.v);
    }
}
