// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 81, lane L4: behaviour gate of
// `CRATONVM_MHN_RESOLVE_METHOD_MEMO` (default off; proposal
// `docs/internal/fixed-bugs/interpreter-i73-L4-proposal-method-member-resolve-memo-and-mirror-judged-load-DONE-20261115.md`).
// The memo keys a method `MemberName`'s resolve by the caller class, the
// lookup mode and the speculative bit as well as the member, so an answer
// found for ONE caller (or mode) must never be served to another. Each round
// below asks the same members from callers and modes that must answer
// differently, three times in alternating order; every round must print the
// same lines, with the switch on and off, in both modes.
//
// Only exception CLASS names are printed (the messages are the JDK's own and
// are covered by L4W23LookupResolveErrors / L4W24LookupFinderMessages).
// Targets and callers are TOP-LEVEL classes (not nestmates of each other).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// r1 priv-own: ok 7
// r1 priv-other: java.lang.IllegalAccessException
// r1 priv-no-private-mode: java.lang.IllegalAccessException
// r1 static-as-virtual: java.lang.IllegalAccessException
// r1 missing: java.lang.NoSuchMethodException
// r1 special-own: ok 7
// r1 special-other: java.lang.IllegalAccessException
// r1 ctor-own: ok
// r1 ctor-other: java.lang.IllegalAccessException
// r1 pub-other: ok 8
// r2 priv-other: java.lang.IllegalAccessException
// r2 priv-own: ok 7
// r2 priv-no-private-mode: java.lang.IllegalAccessException
// r2 static-as-virtual: java.lang.IllegalAccessException
// r2 missing: java.lang.NoSuchMethodException
// r2 special-other: java.lang.IllegalAccessException
// r2 special-own: ok 7
// r2 ctor-other: java.lang.IllegalAccessException
// r2 ctor-own: ok
// r2 pub-other: ok 8
// r3 priv-own: ok 7
// r3 priv-other: java.lang.IllegalAccessException
// r3 priv-no-private-mode: java.lang.IllegalAccessException
// r3 static-as-virtual: java.lang.IllegalAccessException
// r3 missing: java.lang.NoSuchMethodException
// r3 special-own: ok 7
// r3 special-other: java.lang.IllegalAccessException
// r3 ctor-own: ok
// r3 ctor-other: java.lang.IllegalAccessException
// r3 pub-other: ok 8
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W81MethodResolveMemo {
    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = "ok" + suffix(r.run());
        } catch (Throwable t) {
            out = t.getClass().getName();
        }
        System.out.println(name + ": " + out);
    }

    static String suffix(Object o) {
        return o == null ? "" : " " + o;
    }

    static final MethodType INT = MethodType.methodType(int.class);

    static void own(String round) {
        MethodHandles.Lookup lk = L4W81MrmTarget.lookup();
        row(round + " priv-own", () -> {
            MethodHandle mh = lk.findVirtual(L4W81MrmTarget.class, "priv", INT);
            return (int) mh.invoke(new L4W81MrmTarget());
        });
    }

    static void other(String round) {
        MethodHandles.Lookup lk = L4W81MrmCaller.lookup();
        row(round + " priv-other", () -> lk.findVirtual(L4W81MrmTarget.class, "priv", INT) == null ? "null" : null);
    }

    static void specialOwn(String round) {
        MethodHandles.Lookup lk = L4W81MrmTarget.lookup();
        row(round + " special-own", () -> {
            MethodHandle mh = lk.findSpecial(L4W81MrmTarget.class, "priv", INT, L4W81MrmTarget.class);
            return (int) mh.invoke(new L4W81MrmTarget());
        });
    }

    static void specialOther(String round) {
        MethodHandles.Lookup lk = L4W81MrmCaller.lookup();
        row(round + " special-other", () ->
                lk.findSpecial(L4W81MrmTarget.class, "priv", INT, L4W81MrmCaller.class) == null ? "null" : null);
    }

    static void ctorOwn(String round) {
        MethodHandles.Lookup lk = L4W81MrmTarget.lookup();
        row(round + " ctor-own", () ->
                lk.findConstructor(L4W81MrmTarget.class, MethodType.methodType(void.class, int.class)) == null
                        ? "null" : null);
    }

    static void ctorOther(String round) {
        MethodHandles.Lookup lk = L4W81MrmCaller.lookup();
        row(round + " ctor-other", () ->
                lk.findConstructor(L4W81MrmTarget.class, MethodType.methodType(void.class, int.class)) == null
                        ? "null" : null);
    }

    static void common(String round) {
        MethodHandles.Lookup own = L4W81MrmTarget.lookup();
        row(round + " priv-no-private-mode", () ->
                own.dropLookupMode(MethodHandles.Lookup.PRIVATE)
                        .findVirtual(L4W81MrmTarget.class, "priv", INT) == null ? "null" : null);
        row(round + " static-as-virtual", () ->
                own.findVirtual(L4W81MrmTarget.class, "stat", INT) == null ? "null" : null);
        row(round + " missing", () ->
                own.findVirtual(L4W81MrmTarget.class, "absent", INT) == null ? "null" : null);
    }

    static void pub(String round) {
        MethodHandles.Lookup lk = L4W81MrmCaller.lookup();
        row(round + " pub-other", () -> {
            MethodHandle mh = lk.findVirtual(L4W81MrmTarget.class, "pub", INT);
            return (int) mh.invoke(new L4W81MrmTarget());
        });
    }

    public static void main(String[] args) {
        own("r1");
        other("r1");
        common("r1");
        specialOwn("r1");
        specialOther("r1");
        ctorOwn("r1");
        ctorOther("r1");
        pub("r1");

        other("r2");
        own("r2");
        common("r2");
        specialOther("r2");
        specialOwn("r2");
        ctorOther("r2");
        ctorOwn("r2");
        pub("r2");

        own("r3");
        other("r3");
        common("r3");
        specialOwn("r3");
        specialOther("r3");
        ctorOwn("r3");
        ctorOther("r3");
        pub("r3");
    }
}

/** The member owner; top-level, so no other class here is its nestmate. */
class L4W81MrmTarget {
    L4W81MrmTarget() {
    }

    private L4W81MrmTarget(int x) {
    }

    private int priv() {
        return 7;
    }

    public int pub() {
        return 8;
    }

    static int stat() {
        return 9;
    }

    static MethodHandles.Lookup lookup() {
        return MethodHandles.lookup();
    }
}

/** Another caller in the same package, not a nestmate of the target. */
class L4W81MrmCaller {
    static MethodHandles.Lookup lookup() {
        return MethodHandles.lookup();
    }
}
