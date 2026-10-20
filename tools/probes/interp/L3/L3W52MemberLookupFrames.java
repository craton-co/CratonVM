// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 52, lane L3 -- the JDK frames on top of the
// `NoSuchFieldException` / `NoSuchMethodException` a `java.lang.Class` member
// lookup raises (the wave-51 probe `L3W51MemberLookupThrowFrames`, plus the
// shapes it lacks). HotSpot runs the JDK methods; this VM serves most of them
// with Bridges, and since wave 52 a capture standing at such a call lists the
// JDK method at its `throw` (`stackwalker::member_lookup_standin_frames`,
// docs/internal/fixed-bugs/interpreter-L3-a-class-member-lookup-bridge-raises-without-the-jdk-frame-FIXED-20261016.md).
// Each row prints the frames above the probe's first frame as `class.method`
// (no line numbers: they differ between JDK builds), then the first probe
// frame, and whether `getStackTrace()[0].getLineNumber()` is positive.
//
// Rows (the first is repeated as the last, for a cold-VM gap):
//   ctor            -- `getConstructor(int.class)`;
//   field           -- `getField("NOPE")`, a direct call in `main`;
//   declared-field  -- `getDeclaredField("NOPE")`;
//   method          -- `getMethod("nope", int.class)`;
//   declared-method -- `getDeclaredMethod("nope")`;
//   declared-ctor   -- `getDeclaredConstructor(String.class)`;
//   iface-method    -- `Runnable.class.getMethod("nope")` (a boot class);
//   found           -- `getMethod("main", String[].class)`: no exception;
//   hot-field       -- `getField` from a loop run 3000 times (a compiled caller),
//                      the last trace;
//   hot-ctor        -- the same for `getDeclaredConstructor`;
//   ctor-again      -- `ctor` again.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, measured; the same with -Xint)
// ctor: java.lang.NoSuchMethodException [java.lang.Class.getConstructor0 java.lang.Class.getConstructor] L3W52MemberLookupFrames.main line>0=true
// field: java.lang.NoSuchFieldException [java.lang.Class.getField] L3W52MemberLookupFrames.main line>0=true
// declared-field: java.lang.NoSuchFieldException [java.lang.Class.getDeclaredField] L3W52MemberLookupFrames.main line>0=true
// method: java.lang.NoSuchMethodException [java.lang.Class.getMethod] L3W52MemberLookupFrames.main line>0=true
// declared-method: java.lang.NoSuchMethodException [java.lang.Class.getDeclaredMethod] L3W52MemberLookupFrames.main line>0=true
// declared-ctor: java.lang.NoSuchMethodException [java.lang.Class.getConstructor0 java.lang.Class.getDeclaredConstructor] L3W52MemberLookupFrames.main line>0=true
// iface-method: java.lang.NoSuchMethodException [java.lang.Class.getMethod] L3W52MemberLookupFrames.main line>0=true
// found: no exception
// hot-field: java.lang.NoSuchFieldException [java.lang.Class.getField] L3W52MemberLookupFrames.hotField line>0=true
// hot-ctor: java.lang.NoSuchMethodException [java.lang.Class.getConstructor0 java.lang.Class.getDeclaredConstructor] L3W52MemberLookupFrames.hotCtor line>0=true
// ctor-again: java.lang.NoSuchMethodException [java.lang.Class.getConstructor0 java.lang.Class.getConstructor] L3W52MemberLookupFrames.main line>0=true
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W52MemberLookupFrames`.
public class L3W52MemberLookupFrames {
    static void print(String name, Throwable t) {
        if (t == null) {
            System.out.println(name + ": no exception");
            return;
        }
        StackTraceElement[] st = t.getStackTrace();
        StringBuilder jdk = new StringBuilder();
        int i = 0;
        while (i < st.length && !st[i].getClassName().startsWith("L3W52")) {
            jdk.append(i == 0 ? "" : " ").append(st[i].getClassName()).append('.').append(st[i].getMethodName());
            i++;
        }
        String first = i < st.length ? st[i].getClassName() + "." + st[i].getMethodName() : "-";
        System.out.println(name + ": " + t.getClass().getName() + " [" + jdk + "] " + first
                + " line>0=" + (st.length > 0 && st[0].getLineNumber() > 0));
    }

    static Throwable hotField(Class<?> c) {
        try {
            c.getField("NOPE");
            return null;
        } catch (Throwable t) {
            return t;
        }
    }

    static Throwable hotCtor(Class<?> c) {
        try {
            c.getDeclaredConstructor(String.class);
            return null;
        } catch (Throwable t) {
            return t;
        }
    }

    public static void main(String[] args) {
        Class<?> c = L3W52MemberLookupFrames.class;
        Throwable t;
        t = null; try { c.getConstructor(int.class); } catch (Throwable e) { t = e; } print("ctor", t);
        t = null; try { c.getField("NOPE"); } catch (Throwable e) { t = e; } print("field", t);
        t = null; try { c.getDeclaredField("NOPE"); } catch (Throwable e) { t = e; } print("declared-field", t);
        t = null; try { c.getMethod("nope", int.class); } catch (Throwable e) { t = e; } print("method", t);
        t = null; try { c.getDeclaredMethod("nope"); } catch (Throwable e) { t = e; } print("declared-method", t);
        t = null; try { c.getDeclaredConstructor(String.class); } catch (Throwable e) { t = e; } print("declared-ctor", t);
        t = null; try { Runnable.class.getMethod("nope"); } catch (Throwable e) { t = e; } print("iface-method", t);
        t = null; try { c.getMethod("main", String[].class); } catch (Throwable e) { t = e; } print("found", t);
        Throwable last = null;
        for (int i = 0; i < 3000; i++) {
            last = hotField(c);
        }
        print("hot-field", last);
        for (int i = 0; i < 3000; i++) {
            last = hotCtor(c);
        }
        print("hot-ctor", last);
        t = null; try { c.getConstructor(int.class); } catch (Throwable e) { t = e; } print("ctor-again", t);
    }
}
