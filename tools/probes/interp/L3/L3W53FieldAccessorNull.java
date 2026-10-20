// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 53, lane L3 -- the `NullPointerException` a
// `Field` accessor raises for a null receiver of an instance field
// (docs/internal/fixed-bugs/interpreter-L3-a-field-accessor-check-raises-without-the-jdk-frames-FIXED-20261018.md).
// Without the override every accessor fails in `Field.checkAccess`'s
// `obj.getClass()` ("obj"): those rows print the message AND the frames above
// the probe's first frame (`Field.checkAccess` over the accessor; the funnel's
// mark with the implicit NPE's action, `stackwalker::served_raise_frames`).
// With the override, the generic `set` fails in
// `MethodHandleFieldAccessorImpl.ensureObj` ("o") and every other accessor in
// `Objects.requireNonNull` (no message): those rows print class and message
// only (their accessor-chain frames are the page's open half).
//
// Rows (the first is repeated as the last, for a cold-VM gap):
//   get, getInt, set, setInt, ref-set     -- no override, with frames;
//   acc-get, acc-getInt, acc-set, acc-setInt, acc-ref-set, acc-ref-get
//                                         -- after `setAccessible(true)`;
//   get-again                             -- `get` again.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags; the same with -Xint)
// get: java.lang.NullPointerException Cannot invoke "Object.getClass()" because "obj" is null [java.lang.reflect.Field.checkAccess java.lang.reflect.Field.get] L3W53FieldAccessorNull.lambda$main$0
// getInt: java.lang.NullPointerException Cannot invoke "Object.getClass()" because "obj" is null [java.lang.reflect.Field.checkAccess java.lang.reflect.Field.getInt] L3W53FieldAccessorNull.lambda$main$1
// set: java.lang.NullPointerException Cannot invoke "Object.getClass()" because "obj" is null [java.lang.reflect.Field.checkAccess java.lang.reflect.Field.set] L3W53FieldAccessorNull.lambda$main$2
// setInt: java.lang.NullPointerException Cannot invoke "Object.getClass()" because "obj" is null [java.lang.reflect.Field.checkAccess java.lang.reflect.Field.setInt] L3W53FieldAccessorNull.lambda$main$3
// ref-set: java.lang.NullPointerException Cannot invoke "Object.getClass()" because "obj" is null [java.lang.reflect.Field.checkAccess java.lang.reflect.Field.set] L3W53FieldAccessorNull.lambda$main$4
// acc-get: java.lang.NullPointerException null
// acc-getInt: java.lang.NullPointerException null
// acc-set: java.lang.NullPointerException Cannot invoke "Object.getClass()" because "o" is null
// acc-setInt: java.lang.NullPointerException null
// acc-ref-set: java.lang.NullPointerException Cannot invoke "Object.getClass()" because "o" is null
// acc-ref-get: java.lang.NullPointerException null
// get-again: java.lang.NullPointerException Cannot invoke "Object.getClass()" because "obj" is null [java.lang.reflect.Field.checkAccess java.lang.reflect.Field.get] L3W53FieldAccessorNull.lambda$main$11
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W53FieldAccessorNull`.
import java.lang.reflect.Field;

public class L3W53FieldAccessorNull {
    public int f = 1;
    public Object o = "x";

    interface Row {
        void run() throws Throwable;
    }

    static void row(String name, boolean frames, Row r) {
        try {
            r.run();
            System.out.println(name + ": no exception");
        } catch (Throwable t) {
            String line = name + ": " + t.getClass().getName() + " " + t.getMessage();
            if (frames) {
                StackTraceElement[] st = t.getStackTrace();
                StringBuilder jdk = new StringBuilder();
                int i = 0;
                while (i < st.length && !st[i].getClassName().startsWith("L3W53")) {
                    jdk.append(i == 0 ? "" : " ").append(st[i].getClassName()).append('.').append(st[i].getMethodName());
                    i++;
                }
                String first = i < st.length ? st[i].getClassName() + "." + st[i].getMethodName() : "-";
                line += " [" + jdk + "] " + first;
            }
            System.out.println(line);
        }
    }

    public static void main(String[] args) throws Exception {
        Field f = L3W53FieldAccessorNull.class.getField("f");
        Field o = L3W53FieldAccessorNull.class.getField("o");
        row("get", true, () -> f.get(null));
        row("getInt", true, () -> f.getInt(null));
        row("set", true, () -> f.set(null, 2));
        row("setInt", true, () -> f.setInt(null, 2));
        row("ref-set", true, () -> o.set(null, "y"));
        Field fa = L3W53FieldAccessorNull.class.getField("f");
        fa.setAccessible(true);
        Field oa = L3W53FieldAccessorNull.class.getField("o");
        oa.setAccessible(true);
        row("acc-get", false, () -> fa.get(null));
        row("acc-getInt", false, () -> fa.getInt(null));
        row("acc-set", false, () -> fa.set(null, 2));
        row("acc-setInt", false, () -> fa.setInt(null, 2));
        row("acc-ref-set", false, () -> oa.set(null, "y"));
        row("acc-ref-get", false, () -> oa.get(null));
        row("get-again", true, () -> f.get(null));
    }
}
