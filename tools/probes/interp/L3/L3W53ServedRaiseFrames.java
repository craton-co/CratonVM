// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 53, lane L3 -- a native that serves a JDK method
// and raises a throwable lists that JDK method at its throw: the funnel marks
// the raise (`vm_exec::note_served_native_raise`) and the capture rebuilds the
// method from its real bytes (`stackwalker::served_raise_frames`;
// docs/internal/fixed-bugs/interpreter-i52-L3-proposal-a-bridge-that-raises-names-the-jdk-method-it-stands-for-KEPT-20261224.md,
// docs/internal/fixed-bugs/interpreter-L3-a-class-member-lookup-of-a-null-name-raises-the-vms-own-npe-FIXED-20261017.md).
// Each row prints the throwable's class and message, the frames above the
// probe's first frame as `class.method` (no line numbers: they differ between
// JDK builds), then the first probe frame.
//
// Rows (the first is repeated as the last, for a cold-VM gap):
//   field-null           -- `getField(null)`: the Bridge's null-name check;
//   declared-field-null  -- `getDeclaredField(null)`;
//   method-null          -- `getMethod(null)`;
//   declared-method-null -- `getDeclaredMethod(null)`;
//   null-receiver        -- `NULL_CLASS.getField("x")`: the invoke's own NPE,
//                           which must keep the caller on top (the control);
//   miss                 -- `getField("NOPE")`: the member-lookup family's row,
//                           unchanged by the mark;
//   hot-null             -- `getField(null)` from a loop run 3000 times, class
//                           and message only (no frames after a hot loop);
//   field-null-again     -- `field-null` again.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags; the same with -Xint)
// field-null: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.Class.getField] L3W53ServedRaiseFrames.main
// declared-field-null: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.Class.getDeclaredField] L3W53ServedRaiseFrames.main
// method-null: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.Class.getMethod] L3W53ServedRaiseFrames.main
// declared-method-null: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.Class.getDeclaredMethod] L3W53ServedRaiseFrames.main
// null-receiver: java.lang.NullPointerException Cannot invoke "java.lang.Class.getField(String)" because "L3W53ServedRaiseFrames.NULL_CLASS" is null [] L3W53ServedRaiseFrames.main
// miss: java.lang.NoSuchFieldException NOPE [java.lang.Class.getField] L3W53ServedRaiseFrames.main
// hot-null: java.lang.NullPointerException null
// field-null-again: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.Class.getField] L3W53ServedRaiseFrames.main
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W53ServedRaiseFrames`.
public class L3W53ServedRaiseFrames {
    static Class<?> NULL_CLASS;

    static void print(String name, Throwable t) {
        if (t == null) {
            System.out.println(name + ": no exception");
            return;
        }
        StackTraceElement[] st = t.getStackTrace();
        StringBuilder jdk = new StringBuilder();
        int i = 0;
        while (i < st.length && !st[i].getClassName().startsWith("L3W53")) {
            jdk.append(i == 0 ? "" : " ").append(st[i].getClassName()).append('.').append(st[i].getMethodName());
            i++;
        }
        String first = i < st.length ? st[i].getClassName() + "." + st[i].getMethodName() : "-";
        System.out.println(name + ": " + t.getClass().getName() + " " + t.getMessage() + " [" + jdk + "] " + first);
    }

    static Throwable hotNull(Class<?> c) {
        try {
            c.getField(null);
            return null;
        } catch (Throwable t) {
            return t;
        }
    }

    public static void main(String[] args) {
        Class<?> c = L3W53ServedRaiseFrames.class;
        Throwable t;
        t = null; try { c.getField(null); } catch (Throwable e) { t = e; } print("field-null", t);
        t = null; try { c.getDeclaredField(null); } catch (Throwable e) { t = e; } print("declared-field-null", t);
        t = null; try { c.getMethod(null); } catch (Throwable e) { t = e; } print("method-null", t);
        t = null; try { c.getDeclaredMethod(null); } catch (Throwable e) { t = e; } print("declared-method-null", t);
        t = null; try { NULL_CLASS.getField("x"); } catch (Throwable e) { t = e; } print("null-receiver", t);
        t = null; try { c.getField("NOPE"); } catch (Throwable e) { t = e; } print("miss", t);
        Throwable last = null;
        for (int i = 0; i < 3000; i++) {
            last = hotNull(c);
        }
        System.out.println("hot-null: " + (last == null ? "no exception" : last.getClass().getName() + " " + last.getMessage()));
        t = null; try { c.getField(null); } catch (Throwable e) { t = e; } print("field-null-again", t);
    }
}
