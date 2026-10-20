// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 54, lane L3 -- the open half of
// docs/known-issues/interpreter/i52-L3-a-field-accessor-check-raises-without-the-jdk-frames-20261016.md:
// the frames of a `Field` accessor's raise that HotSpot lists inside the JDK
// field accessor (`MethodHandle<Type>FieldAccessorImpl`), several hops below
// the `Field` method the VM serves. The served `Field` native notes the
// chain (`NativeExceptionAccess::note_served_raise_chain`) and the funnel's
// mark carries it (`stackwalker::served_raise_chain_frames`).
//
// Each row prints the throwable's class and message, then the frames above
// the probe's first frame as `class.method` (no line numbers), then the
// first probe frame.
//
// Rows (the first is repeated as the last, for a cold-VM gap):
//   acc-*        -- after `setAccessible(true)`, a null receiver of an
//                   instance field: `get`/`getInt`/`getLong` of an `int`,
//                   `get` of a `long`, `char` and `Object` field, `setInt`
//                   and `setLong` of an `int` / `long`, the generic `set`;
//   wrong-*      -- a receiver of the wrong class (a `String`), without and
//                   with the override: `get`, `getInt`, `set`, `setInt`.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags; the same with -Xint)
// acc-get-int: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.invoke.DirectMethodHandle.checkBase jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.getInt jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.get java.lang.reflect.Field.get] L3W54FieldOverrideAccessorFrames.lambda$main$0
// acc-getInt-int: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.invoke.DirectMethodHandle.checkBase jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.getInt java.lang.reflect.Field.getInt] L3W54FieldOverrideAccessorFrames.lambda$main$1
// acc-getLong-int: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.invoke.DirectMethodHandle.checkBase jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.getInt jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.getLong java.lang.reflect.Field.getLong] L3W54FieldOverrideAccessorFrames.lambda$main$2
// acc-get-long: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.invoke.DirectMethodHandle.checkBase jdk.internal.reflect.MethodHandleLongFieldAccessorImpl.getLong jdk.internal.reflect.MethodHandleLongFieldAccessorImpl.get java.lang.reflect.Field.get] L3W54FieldOverrideAccessorFrames.lambda$main$3
// acc-get-char: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.invoke.DirectMethodHandle.checkBase jdk.internal.reflect.MethodHandleCharacterFieldAccessorImpl.getChar jdk.internal.reflect.MethodHandleCharacterFieldAccessorImpl.get java.lang.reflect.Field.get] L3W54FieldOverrideAccessorFrames.lambda$main$4
// acc-get-object: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.invoke.DirectMethodHandle.checkBase jdk.internal.reflect.MethodHandleObjectFieldAccessorImpl.get java.lang.reflect.Field.get] L3W54FieldOverrideAccessorFrames.lambda$main$5
// acc-setInt-int: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.invoke.DirectMethodHandle.checkBase jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.setInt java.lang.reflect.Field.setInt] L3W54FieldOverrideAccessorFrames.lambda$main$6
// acc-setInt-long: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.invoke.DirectMethodHandle.checkBase jdk.internal.reflect.MethodHandleLongFieldAccessorImpl.setLong jdk.internal.reflect.MethodHandleLongFieldAccessorImpl.setInt java.lang.reflect.Field.setInt] L3W54FieldOverrideAccessorFrames.lambda$main$7
// acc-setLong-long: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.invoke.DirectMethodHandle.checkBase jdk.internal.reflect.MethodHandleLongFieldAccessorImpl.setLong java.lang.reflect.Field.setLong] L3W54FieldOverrideAccessorFrames.lambda$main$8
// acc-set-int: java.lang.NullPointerException Cannot invoke "Object.getClass()" because "o" is null [jdk.internal.reflect.MethodHandleFieldAccessorImpl.ensureObj jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.set java.lang.reflect.Field.set] L3W54FieldOverrideAccessorFrames.lambda$main$9
// acc-set-object: java.lang.NullPointerException Cannot invoke "Object.getClass()" because "o" is null [jdk.internal.reflect.MethodHandleFieldAccessorImpl.ensureObj jdk.internal.reflect.MethodHandleObjectFieldAccessorImpl.set java.lang.reflect.Field.set] L3W54FieldOverrideAccessorFrames.lambda$main$10
// wrong-get: java.lang.IllegalArgumentException Can not get int field L3W54FieldOverrideAccessorFrames.i on java.lang.String [jdk.internal.reflect.MethodHandleFieldAccessorImpl.newGetIllegalArgumentException jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.getInt jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.get java.lang.reflect.Field.get] L3W54FieldOverrideAccessorFrames.lambda$main$11
// wrong-getInt: java.lang.IllegalArgumentException Can not get int field L3W54FieldOverrideAccessorFrames.i on java.lang.String [jdk.internal.reflect.MethodHandleFieldAccessorImpl.newGetIllegalArgumentException jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.getInt java.lang.reflect.Field.getInt] L3W54FieldOverrideAccessorFrames.lambda$main$12
// wrong-set: java.lang.IllegalArgumentException Can not set int field L3W54FieldOverrideAccessorFrames.i to java.lang.String [jdk.internal.reflect.FieldAccessorImpl.throwSetIllegalArgumentException jdk.internal.reflect.FieldAccessorImpl.throwSetIllegalArgumentException jdk.internal.reflect.MethodHandleFieldAccessorImpl.ensureObj jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.set java.lang.reflect.Field.set] L3W54FieldOverrideAccessorFrames.lambda$main$13
// wrong-setInt: java.lang.IllegalArgumentException Can not set int field L3W54FieldOverrideAccessorFrames.i on java.lang.String [jdk.internal.reflect.MethodHandleFieldAccessorImpl.newSetIllegalArgumentException jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.setInt java.lang.reflect.Field.setInt] L3W54FieldOverrideAccessorFrames.lambda$main$14
// acc-wrong-get: java.lang.IllegalArgumentException Can not get int field L3W54FieldOverrideAccessorFrames.i on java.lang.String [jdk.internal.reflect.MethodHandleFieldAccessorImpl.newGetIllegalArgumentException jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.getInt jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.get java.lang.reflect.Field.get] L3W54FieldOverrideAccessorFrames.lambda$main$15
// acc-wrong-getLong: java.lang.IllegalArgumentException Can not get int field L3W54FieldOverrideAccessorFrames.i on java.lang.String [jdk.internal.reflect.MethodHandleFieldAccessorImpl.newGetIllegalArgumentException jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.getInt jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.getLong java.lang.reflect.Field.getLong] L3W54FieldOverrideAccessorFrames.lambda$main$16
// acc-wrong-set: java.lang.IllegalArgumentException Can not set int field L3W54FieldOverrideAccessorFrames.i to java.lang.String [jdk.internal.reflect.FieldAccessorImpl.throwSetIllegalArgumentException jdk.internal.reflect.FieldAccessorImpl.throwSetIllegalArgumentException jdk.internal.reflect.MethodHandleFieldAccessorImpl.ensureObj jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.set java.lang.reflect.Field.set] L3W54FieldOverrideAccessorFrames.lambda$main$17
// acc-wrong-setInt: java.lang.IllegalArgumentException Can not set int field L3W54FieldOverrideAccessorFrames.i on java.lang.String [jdk.internal.reflect.MethodHandleFieldAccessorImpl.newSetIllegalArgumentException jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.setInt java.lang.reflect.Field.setInt] L3W54FieldOverrideAccessorFrames.lambda$main$18
// acc-get-int-again: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.invoke.DirectMethodHandle.checkBase jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.getInt jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.get java.lang.reflect.Field.get] L3W54FieldOverrideAccessorFrames.lambda$main$19
// HOTSPOT_EXPECTED_END
//
// CratonVM before wave 54 printed every row with no JDK frame (`[]`); the
// no-override null-receiver rows are `L3W53FieldAccessorNull`'s.
// Positive control: `CRATONVM_FRAME_TRACE=1` prints
//     [SERVED_RAISE_FRAMES] java/lang/reflect/Field.get(...) raised ... (chain of <n>): <n> frame(s)
// per row.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W54FieldOverrideAccessorFrames`.
import java.lang.reflect.Field;

public class L3W54FieldOverrideAccessorFrames {
    public int i = 1;
    public long l = 2L;
    public char c = 'c';
    public Object o = "x";

    interface Row {
        void run() throws Throwable;
    }

    static void row(String name, Row r) {
        try {
            r.run();
            System.out.println(name + ": no exception");
        } catch (Throwable t) {
            StackTraceElement[] st = t.getStackTrace();
            StringBuilder jdk = new StringBuilder();
            int k = 0;
            while (k < st.length && !st[k].getClassName().startsWith("L3W54")) {
                jdk.append(k == 0 ? "" : " ").append(st[k].getClassName()).append('.').append(st[k].getMethodName());
                k++;
            }
            String first = k < st.length ? st[k].getClassName() + "." + st[k].getMethodName() : "-";
            System.out.println(name + ": " + t.getClass().getName() + " " + t.getMessage() + " [" + jdk + "] " + first);
        }
    }

    static Field field(String name, boolean accessible) throws Exception {
        Field f = L3W54FieldOverrideAccessorFrames.class.getField(name);
        if (accessible) {
            f.setAccessible(true);
        }
        return f;
    }

    public static void main(String[] args) throws Exception {
        Field ai = field("i", true);
        Field al = field("l", true);
        Field ac = field("c", true);
        Field ao = field("o", true);
        Field pi = field("i", false);
        row("acc-get-int", () -> ai.get(null));
        row("acc-getInt-int", () -> ai.getInt(null));
        row("acc-getLong-int", () -> ai.getLong(null));
        row("acc-get-long", () -> al.get(null));
        row("acc-get-char", () -> ac.get(null));
        row("acc-get-object", () -> ao.get(null));
        row("acc-setInt-int", () -> ai.setInt(null, 2));
        row("acc-setInt-long", () -> al.setInt(null, 2));
        row("acc-setLong-long", () -> al.setLong(null, 2L));
        row("acc-set-int", () -> ai.set(null, 2));
        row("acc-set-object", () -> ao.set(null, "y"));
        row("wrong-get", () -> pi.get("wrong"));
        row("wrong-getInt", () -> pi.getInt("wrong"));
        row("wrong-set", () -> pi.set("wrong", 2));
        row("wrong-setInt", () -> pi.setInt("wrong", 2));
        row("acc-wrong-get", () -> ai.get("wrong"));
        row("acc-wrong-getLong", () -> ai.getLong("wrong"));
        row("acc-wrong-set", () -> ai.set("wrong", 2));
        row("acc-wrong-setInt", () -> ai.setInt("wrong", 2));
        row("acc-get-int-again", () -> ai.get(null));
    }
}
