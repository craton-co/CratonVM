// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 71, lane L4 -- two default-off switches:
//   CRATONVM_MH_FINDER_SHARED_MEMBER_WORDS=1 (proposal
//     `i70-L4-proposal-finder-handles-share-their-member-words-20261103`):
//     a finder's field getter / setter takes its member strings from the
//     intern pool and its `type` from the shared MethodType table, so every
//     fresh finder handle of one field holds the same words (and the
//     link-derivation memo can hit on them).
//   CRATONVM_MH_LINKING_CALL_DIRECT_SERVE=1 (with
//     CRATONVM_MH_LINKING_CALL_SERVED=1; proposals
//     `i70-L4-proposal-a-memo-hit-serves-the-linking-call-20261103` and
//     `i68-L4-proposal-a-linking-call-is-served-by-its-link-20261101`):
//     the call that links a field accessor is served from what the link
//     attempt admitted.
//
// Every row's handle is FRESH (a new finder handle, or a new bound copy), so
// each call below is a linking call, and the rows change one thing the link
// must judge: the receiver's class, a null receiver, an unrelated receiver
// through `invoke`, the call site's type, a boxed / widened conversion, a
// checked reference value, a static field's class initialization.
//
// The two `find-type-*` rows are BEHAVIOUR the first switch changes: HotSpot
// interns `MethodType`s (`MethodType.methodType` answers one object per
// type), so two finder handles of one field type answer `a.type() ==
// b.type()`. With the switch off this VM builds a type per handle and both
// rows print `false` (DIFF(2), the positive control); filed as
// `i71-L4-finder-handles-of-one-type-do-not-share-their-method-type-20261104.md`.
// Every other row must be SAME in every switch combination, in both modes.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// find-get-exact: 20
// find-get-invoke: 20
// find-get-sub: 60
// find-get-mixed: 35
// find-get-null: java.lang.NullPointerException
// find-get-wmte-sub: java.lang.invoke.WrongMethodTypeException: handle's method type (Base)int but found (Sub)int
// find-get-object-invoke: 1
// find-get-string-invoke: java.lang.ClassCastException: Cannot cast java.lang.String to L4W71FinderLinkServe$Base
// find-set-exact: 9
// find-set-invoke: 109
// find-set-boxed-invoke: 8
// find-ref-set-classes: sb0;st1;null;sb3;st4;null;
// find-ref-set-wrong-object: java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.CharSequence
// find-type-same: true
// find-type-other-field: true
// find-type-text: (Base)int
// bound-get-exact: 20
// bound-get-invoke: 20
// bound-get-mixed: 35
// bound-set-exact: 9
// bound-get-wmte-long: java.lang.invoke.WrongMethodTypeException: handle's method type ()int but found ()long
// bound-get-long-invoke: 1
// static-get-first-init: clinit;21
// static-set-first: 12
// HOTSPOT_EXPECTED_END
//
// Switch sets, `--compatible` and `--jdk-only` (each with `--nojit` too):
//   (none)                                                   [find-type-* DIFF]
//   CRATONVM_MH_FINDER_SHARED_MEMBER_WORDS=1                 [all SAME]
//   CRATONVM_MH_LINKING_CALL_SERVED=1 CRATONVM_MH_LINKING_CALL_DIRECT_SERVE=1
//   all three, plus CRATONVM_MH_FIELD_LINK_DERIVATION_MEMO=1
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L4W71FinderLinkServe`.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;

public class L4W71FinderLinkServe {
    public static class Base {
        public int v = 1;
        public int w = 9;
        public CharSequence cs = "init";
    }

    public static class Sub extends Base {
    }

    static final StringBuilder LOG = new StringBuilder();

    public static class Lazy {
        public static int X;

        static {
            LOG.append("clinit;");
            X = 7;
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

    static Base base = new Base();
    static Sub sub = new Sub();

    static void reset() {
        base.v = 1;
        sub.v = 6;
        base.cs = "init";
        sub.cs = "init";
    }

    static MethodHandle getter() throws Throwable {
        return LK.findGetter(Base.class, "v", int.class);
    }

    static MethodHandle setter() throws Throwable {
        return LK.findSetter(Base.class, "v", int.class);
    }

    public static void main(String[] args) throws Throwable {
        reset();

        // ---- fresh finder handles: every call links ----
        row("find-get-exact", () -> {
            long s = 0;
            for (int i = 0; i < 20; i++) s += (int) getter().invokeExact(base);
            return s;
        });
        row("find-get-invoke", () -> {
            long s = 0;
            for (int i = 0; i < 20; i++) s += (int) getter().invoke(base);
            return s;
        });
        row("find-get-sub", () -> {
            long s = 0;
            for (int i = 0; i < 10; i++) s += (int) getter().invokeExact((Base) sub);
            return s;
        });
        row("find-get-mixed", () -> {
            long s = 0;
            for (int i = 0; i < 10; i++) s += (int) getter().invokeExact(i % 2 == 0 ? base : (Base) sub);
            return s;
        });
        row("find-get-null", () -> (int) getter().invokeExact((Base) null));
        row("find-get-wmte-sub", () -> (int) getter().invokeExact(sub));
        row("find-get-object-invoke", () -> (Object) getter().invoke(base));
        row("find-get-string-invoke", () -> (int) getter().invoke((Object) "x"));

        row("find-set-exact", () -> {
            for (int i = 0; i < 10; i++) setter().invokeExact(base, i);
            return base.v;
        });
        reset();
        row("find-set-invoke", () -> {
            for (int i = 0; i < 10; i++) setter().invoke(base, i + 100);
            return base.v;
        });
        reset();
        row("find-set-boxed-invoke", () -> {
            for (int i = 0; i < 3; i++) setter().invoke(base, Integer.valueOf(8));
            return base.v;
        });
        reset();
        row("find-ref-set-classes", () -> {
            StringBuilder out = new StringBuilder();
            for (int i = 0; i < 6; i++) {
                Base r = i % 2 == 0 ? base : sub;
                CharSequence v = i % 3 == 0 ? new StringBuilder("sb" + i) : (i % 3 == 1 ? "st" + i : null);
                LK.findSetter(Base.class, "cs", CharSequence.class).invokeExact(r, v);
                out.append(r.cs).append(';');
            }
            return out;
        });
        row("find-ref-set-wrong-object", () -> {
            LK.findSetter(Base.class, "cs", CharSequence.class).invoke(base, (Object) Integer.valueOf(1));
            return base.cs;
        });
        reset();

        // ---- what two finder handles share ----
        row("find-type-same", () -> getter().type() == getter().type());
        row("find-type-other-field", () ->
                getter().type() == LK.findGetter(Base.class, "w", int.class).type());
        row("find-type-text", () -> getter().type());

        // ---- fresh bound copies of one source ----
        MethodHandle get = getter();
        MethodHandle set = setter();
        row("bound-get-exact", () -> {
            long s = 0;
            for (int i = 0; i < 20; i++) s += (int) get.bindTo(base).invokeExact();
            return s;
        });
        row("bound-get-invoke", () -> {
            long s = 0;
            for (int i = 0; i < 20; i++) s += (int) get.bindTo(base).invoke();
            return s;
        });
        row("bound-get-mixed", () -> {
            long s = 0;
            for (int i = 0; i < 10; i++) s += (int) get.bindTo(i % 2 == 0 ? base : sub).invokeExact();
            return s;
        });
        row("bound-set-exact", () -> {
            for (int i = 0; i < 10; i++) set.bindTo(base).invokeExact(i);
            return base.v;
        });
        reset();
        row("bound-get-wmte-long", () -> (long) get.bindTo(base).invokeExact());
        row("bound-get-long-invoke", () -> (long) get.bindTo(base).invoke());

        // ---- statics: the first call initializes the class ----
        row("static-get-first-init", () -> {
            long s = 0;
            for (int i = 0; i < 3; i++) {
                s += (int) LK.findStaticGetter(Lazy.class, "X", int.class).invokeExact();
            }
            return LOG.toString() + s;
        });
        row("static-set-first", () -> {
            for (int i = 0; i < 3; i++) LK.findStaticSetter(Lazy.class, "X", int.class).invokeExact(i + 10);
            return Lazy.X;
        });
    }
}
