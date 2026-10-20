// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 57, lane L3 --
// docs/internal/fixed-bugs/interpreter-L3-compatible-field-refusals-list-no-accessor-frames-FIXED-20261021.md:
// the frames HotSpot lists inside the JDK field accessor
// (`MethodHandle<Type>FieldAccessorImpl` / `FieldAccessorImpl`) for a `Field`
// refusal that is not about the receiver: a final field's write, a value of
// the wrong type, a getter's illegal conversion. `--compatible`'s `Field`
// natives serve them and note the chain
// (`lang_class::field_raise_chain`); `--jdk-only` runs the accessor itself.
//
// Each row prints the throwable's class and message, then the frames above
// the probe's first frame as `class.method` (no line numbers), then the
// first probe frame. The first row is repeated as the last (cold-VM gap).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags; the same with -Xint)
// final-set: java.lang.IllegalAccessException Can not set static final int field L3W57FieldRefusalFrames.SFI to java.lang.Integer [jdk.internal.reflect.FieldAccessorImpl.throwFinalFieldIllegalAccessException jdk.internal.reflect.FieldAccessorImpl.throwFinalFieldIllegalAccessException jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.set java.lang.reflect.Field.set] L3W57FieldRefusalFrames.lambda$main$0
// final-setInt: java.lang.IllegalAccessException Can not set static final int field L3W57FieldRefusalFrames.SFI to (int)5 [jdk.internal.reflect.FieldAccessorImpl.throwFinalFieldIllegalAccessException jdk.internal.reflect.FieldAccessorImpl.throwFinalFieldIllegalAccessException jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.setInt java.lang.reflect.Field.setInt] L3W57FieldRefusalFrames.lambda$main$1
// final-setByte: java.lang.IllegalAccessException Can not set static final int field L3W57FieldRefusalFrames.SFI to (int)5 [jdk.internal.reflect.FieldAccessorImpl.throwFinalFieldIllegalAccessException jdk.internal.reflect.FieldAccessorImpl.throwFinalFieldIllegalAccessException jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.setInt jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.setByte java.lang.reflect.Field.setByte] L3W57FieldRefusalFrames.lambda$main$2
// final-setChar: java.lang.IllegalAccessException Can not set static final int field L3W57FieldRefusalFrames.SFI to (int)122 [jdk.internal.reflect.FieldAccessorImpl.throwFinalFieldIllegalAccessException jdk.internal.reflect.FieldAccessorImpl.throwFinalFieldIllegalAccessException jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.setInt jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.setChar java.lang.reflect.Field.setChar] L3W57FieldRefusalFrames.lambda$main$3
// final-acc-setInt: java.lang.IllegalAccessException Can not set static final int field L3W57FieldRefusalFrames.SFI to (int)5 [jdk.internal.reflect.FieldAccessorImpl.throwFinalFieldIllegalAccessException jdk.internal.reflect.FieldAccessorImpl.throwFinalFieldIllegalAccessException jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.setInt java.lang.reflect.Field.setInt] L3W57FieldRefusalFrames.lambda$main$4
// final-setInt-long: java.lang.IllegalAccessException Can not set static final long field L3W57FieldRefusalFrames.SFL to (long)5 [jdk.internal.reflect.FieldAccessorImpl.throwFinalFieldIllegalAccessException jdk.internal.reflect.FieldAccessorImpl.throwFinalFieldIllegalAccessException jdk.internal.reflect.MethodHandleLongFieldAccessorImpl.setLong jdk.internal.reflect.MethodHandleLongFieldAccessorImpl.setInt java.lang.reflect.Field.setInt] L3W57FieldRefusalFrames.lambda$main$5
// final-set-null: java.lang.IllegalAccessException Can not set static final int field L3W57FieldRefusalFrames.SFI to null value [jdk.internal.reflect.FieldAccessorImpl.throwFinalFieldIllegalAccessException jdk.internal.reflect.FieldAccessorImpl.throwFinalFieldIllegalAccessException jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.set java.lang.reflect.Field.set] L3W57FieldRefusalFrames.lambda$main$6
// final-set-string: java.lang.IllegalAccessException Can not set static final java.lang.String field L3W57FieldRefusalFrames.SFS to java.lang.String [jdk.internal.reflect.FieldAccessorImpl.throwFinalFieldIllegalAccessException jdk.internal.reflect.FieldAccessorImpl.throwFinalFieldIllegalAccessException jdk.internal.reflect.MethodHandleObjectFieldAccessorImpl.set java.lang.reflect.Field.set] L3W57FieldRefusalFrames.lambda$main$7
// final-inst-set: java.lang.IllegalAccessException Can not set final int field L3W57FieldRefusalFrames.fi to java.lang.Integer [jdk.internal.reflect.FieldAccessorImpl.throwFinalFieldIllegalAccessException jdk.internal.reflect.FieldAccessorImpl.throwFinalFieldIllegalAccessException jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.set java.lang.reflect.Field.set] L3W57FieldRefusalFrames.lambda$main$8
// final-inst-setInt: java.lang.IllegalAccessException Can not set final int field L3W57FieldRefusalFrames.fi to (int)7 [jdk.internal.reflect.FieldAccessorImpl.throwFinalFieldIllegalAccessException jdk.internal.reflect.FieldAccessorImpl.throwFinalFieldIllegalAccessException jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.setInt java.lang.reflect.Field.setInt] L3W57FieldRefusalFrames.lambda$main$9
// value-set: java.lang.IllegalArgumentException Can not set static int field L3W57FieldRefusalFrames.si to java.lang.String [jdk.internal.reflect.FieldAccessorImpl.throwSetIllegalArgumentException jdk.internal.reflect.FieldAccessorImpl.throwSetIllegalArgumentException jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.set java.lang.reflect.Field.set] L3W57FieldRefusalFrames.lambda$main$10
// value-set-null: java.lang.IllegalArgumentException Can not set static int field L3W57FieldRefusalFrames.si to null value [jdk.internal.reflect.FieldAccessorImpl.throwSetIllegalArgumentException jdk.internal.reflect.FieldAccessorImpl.throwSetIllegalArgumentException jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.set java.lang.reflect.Field.set] L3W57FieldRefusalFrames.lambda$main$11
// value-set-long: java.lang.IllegalArgumentException Can not set static int field L3W57FieldRefusalFrames.si to java.lang.Long [jdk.internal.reflect.FieldAccessorImpl.throwSetIllegalArgumentException jdk.internal.reflect.FieldAccessorImpl.throwSetIllegalArgumentException jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.set java.lang.reflect.Field.set] L3W57FieldRefusalFrames.lambda$main$12
// value-setLong: java.lang.IllegalArgumentException Can not set static int field L3W57FieldRefusalFrames.si to (long)5 [jdk.internal.reflect.FieldAccessorImpl.throwSetIllegalArgumentException jdk.internal.reflect.FieldAccessorImpl.throwSetIllegalArgumentException jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.setLong java.lang.reflect.Field.setLong] L3W57FieldRefusalFrames.lambda$main$13
// value-setBoolean: java.lang.IllegalArgumentException Can not set static int field L3W57FieldRefusalFrames.si to (boolean)true [jdk.internal.reflect.FieldAccessorImpl.throwSetIllegalArgumentException jdk.internal.reflect.FieldAccessorImpl.throwSetIllegalArgumentException jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.setBoolean java.lang.reflect.Field.setBoolean] L3W57FieldRefusalFrames.lambda$main$14
// value-setInt-boolean: java.lang.IllegalArgumentException Can not set static boolean field L3W57FieldRefusalFrames.sz to (int)1 [jdk.internal.reflect.FieldAccessorImpl.throwSetIllegalArgumentException jdk.internal.reflect.FieldAccessorImpl.throwSetIllegalArgumentException jdk.internal.reflect.MethodHandleBooleanFieldAccessorImpl.setInt java.lang.reflect.Field.setInt] L3W57FieldRefusalFrames.lambda$main$15
// value-setInt-object: java.lang.IllegalArgumentException Can not set static java.lang.Object field L3W57FieldRefusalFrames.so to (int)1 [jdk.internal.reflect.FieldAccessorImpl.throwSetIllegalArgumentException jdk.internal.reflect.FieldAccessorImpl.throwSetIllegalArgumentException jdk.internal.reflect.MethodHandleObjectFieldAccessorImpl.setInt java.lang.reflect.Field.setInt] L3W57FieldRefusalFrames.lambda$main$16
// value-set-integer: java.lang.IllegalArgumentException Can not set static java.lang.Integer field L3W57FieldRefusalFrames.sint to java.lang.String [jdk.internal.reflect.FieldAccessorImpl.throwSetIllegalArgumentException jdk.internal.reflect.FieldAccessorImpl.throwSetIllegalArgumentException jdk.internal.reflect.MethodHandleObjectFieldAccessorImpl.set java.lang.reflect.Field.set] L3W57FieldRefusalFrames.lambda$main$17
// value-acc-setDouble: java.lang.IllegalArgumentException Can not set int field L3W57FieldRefusalFrames.i to (double)1.5 [jdk.internal.reflect.FieldAccessorImpl.throwSetIllegalArgumentException jdk.internal.reflect.FieldAccessorImpl.throwSetIllegalArgumentException jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.setDouble java.lang.reflect.Field.setDouble] L3W57FieldRefusalFrames.lambda$main$18
// value-final-setLong: java.lang.IllegalArgumentException Can not set static final int field L3W57FieldRefusalFrames.SFI to (long)5 [jdk.internal.reflect.FieldAccessorImpl.throwSetIllegalArgumentException jdk.internal.reflect.FieldAccessorImpl.throwSetIllegalArgumentException jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.setLong java.lang.reflect.Field.setLong] L3W57FieldRefusalFrames.lambda$main$19
// get-getByte: java.lang.IllegalArgumentException Attempt to get int field "L3W57FieldRefusalFrames.si" with illegal data type conversion to byte [jdk.internal.reflect.FieldAccessorImpl.newGetIllegalArgumentException jdk.internal.reflect.FieldAccessorImpl.newGetByteIllegalArgumentException jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.getByte java.lang.reflect.Field.getByte] L3W57FieldRefusalFrames.lambda$main$20
// get-getInt-long: java.lang.IllegalArgumentException Attempt to get long field "L3W57FieldRefusalFrames.sl" with illegal data type conversion to int [jdk.internal.reflect.FieldAccessorImpl.newGetIllegalArgumentException jdk.internal.reflect.FieldAccessorImpl.newGetIntIllegalArgumentException jdk.internal.reflect.MethodHandleLongFieldAccessorImpl.getInt java.lang.reflect.Field.getInt] L3W57FieldRefusalFrames.lambda$main$21
// get-getInt-object: java.lang.IllegalArgumentException Attempt to get java.lang.Object field "L3W57FieldRefusalFrames.so" with illegal data type conversion to int [jdk.internal.reflect.FieldAccessorImpl.newGetIllegalArgumentException jdk.internal.reflect.FieldAccessorImpl.newGetIntIllegalArgumentException jdk.internal.reflect.MethodHandleObjectFieldAccessorImpl.getInt java.lang.reflect.Field.getInt] L3W57FieldRefusalFrames.lambda$main$22
// get-getBoolean: java.lang.IllegalArgumentException Attempt to get int field "L3W57FieldRefusalFrames.si" with illegal data type conversion to boolean [jdk.internal.reflect.FieldAccessorImpl.newGetIllegalArgumentException jdk.internal.reflect.FieldAccessorImpl.newGetBooleanIllegalArgumentException jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.getBoolean java.lang.reflect.Field.getBoolean] L3W57FieldRefusalFrames.lambda$main$23
// get-acc-getShort: java.lang.IllegalArgumentException Attempt to get int field "L3W57FieldRefusalFrames.i" with illegal data type conversion to short [jdk.internal.reflect.FieldAccessorImpl.newGetIllegalArgumentException jdk.internal.reflect.FieldAccessorImpl.newGetShortIllegalArgumentException jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.getShort java.lang.reflect.Field.getShort] L3W57FieldRefusalFrames.lambda$main$24
// get-getChar-boolean: java.lang.IllegalArgumentException Attempt to get boolean field "L3W57FieldRefusalFrames.sz" with illegal data type conversion to char [jdk.internal.reflect.FieldAccessorImpl.newGetIllegalArgumentException jdk.internal.reflect.FieldAccessorImpl.newGetCharIllegalArgumentException jdk.internal.reflect.MethodHandleBooleanFieldAccessorImpl.getChar java.lang.reflect.Field.getChar] L3W57FieldRefusalFrames.lambda$main$25
// ro-setInt-wrong: java.lang.IllegalArgumentException Can not set final int field L3W57FieldRefusalFrames.fi to java.lang.String [jdk.internal.reflect.FieldAccessorImpl.throwSetIllegalArgumentException jdk.internal.reflect.FieldAccessorImpl.throwSetIllegalArgumentException jdk.internal.reflect.MethodHandleFieldAccessorImpl.ensureObj jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.setInt java.lang.reflect.Field.setInt] L3W57FieldRefusalFrames.lambda$main$26
// ro-setByte-wrong: java.lang.IllegalArgumentException Can not set final int field L3W57FieldRefusalFrames.fi to java.lang.String [jdk.internal.reflect.FieldAccessorImpl.throwSetIllegalArgumentException jdk.internal.reflect.FieldAccessorImpl.throwSetIllegalArgumentException jdk.internal.reflect.MethodHandleFieldAccessorImpl.ensureObj jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.setInt jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.setByte java.lang.reflect.Field.setByte] L3W57FieldRefusalFrames.lambda$main$27
// ro-setLong-wrong: java.lang.IllegalArgumentException Can not set final int field L3W57FieldRefusalFrames.fi to (long)5 [jdk.internal.reflect.FieldAccessorImpl.throwSetIllegalArgumentException jdk.internal.reflect.FieldAccessorImpl.throwSetIllegalArgumentException jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.setLong java.lang.reflect.Field.setLong] L3W57FieldRefusalFrames.lambda$main$28
// rec-acc-setInt-null: java.lang.NullPointerException Cannot invoke "Object.getClass()" because "o" is null [jdk.internal.reflect.MethodHandleFieldAccessorImpl.ensureObj jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.setInt java.lang.reflect.Field.setInt] L3W57FieldRefusalFrames.lambda$main$29
// rec-acc-setShort-null: java.lang.NullPointerException Cannot invoke "Object.getClass()" because "o" is null [jdk.internal.reflect.MethodHandleFieldAccessorImpl.ensureObj jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.setInt jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.setShort java.lang.reflect.Field.setShort] L3W57FieldRefusalFrames.lambda$main$30
// rec-acc-setInt-wrong: java.lang.IllegalArgumentException Can not set final int field L3W57FieldRefusalFrames$Rec.x to java.lang.String [jdk.internal.reflect.FieldAccessorImpl.throwSetIllegalArgumentException jdk.internal.reflect.FieldAccessorImpl.throwSetIllegalArgumentException jdk.internal.reflect.MethodHandleFieldAccessorImpl.ensureObj jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.setInt java.lang.reflect.Field.setInt] L3W57FieldRefusalFrames.lambda$main$31
// rec-acc-set-null: java.lang.NullPointerException Cannot invoke "Object.getClass()" because "o" is null [jdk.internal.reflect.MethodHandleFieldAccessorImpl.ensureObj jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.set java.lang.reflect.Field.set] L3W57FieldRefusalFrames.lambda$main$32
// final-set-again: java.lang.IllegalAccessException Can not set static final int field L3W57FieldRefusalFrames.SFI to java.lang.Integer [jdk.internal.reflect.FieldAccessorImpl.throwFinalFieldIllegalAccessException jdk.internal.reflect.FieldAccessorImpl.throwFinalFieldIllegalAccessException jdk.internal.reflect.MethodHandleIntegerFieldAccessorImpl.set java.lang.reflect.Field.set] L3W57FieldRefusalFrames.lambda$main$33
// HOTSPOT_EXPECTED_END
//
// CratonVM before wave 57 (`--compatible`): every row's frame list is `[]`.
// Positive control: `CRATONVM_FRAME_TRACE=1` prints a
// `[SERVED_RAISE_FRAMES] ... (chain of <n>)` line per row.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W57FieldRefusalFrames`.
import java.lang.reflect.Field;

public class L3W57FieldRefusalFrames {
    public static final int SFI = 1;
    public static final String SFS = "s";
    public static final long SFL = 3L;
    public static int si = 1;
    public static long sl = 2L;
    public static Object so = "x";
    public static Integer sint = 4;
    public static boolean sz = true;
    public final int fi = 5;
    public int i = 6;

    record Rec(int x) {
    }

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
            while (k < st.length && !st[k].getClassName().startsWith("L3W57")) {
                jdk.append(k == 0 ? "" : " ").append(st[k].getClassName()).append('.').append(st[k].getMethodName());
                k++;
            }
            String first = k < st.length ? st[k].getClassName() + "." + st[k].getMethodName() : "-";
            System.out.println(name + ": " + t.getClass().getName() + " " + t.getMessage() + " [" + jdk + "] " + first);
        }
    }

    static Field field(String name, boolean accessible) throws Exception {
        Field f = L3W57FieldRefusalFrames.class.getField(name);
        if (accessible) {
            f.setAccessible(true);
        }
        return f;
    }

    public static void main(String[] args) throws Exception {
        Field sfi = field("SFI", false);
        Field sfs = field("SFS", false);
        Field asfi = field("SFI", true);
        Field sfl = field("SFL", false);
        Field si = field("si", false);
        Field sl = field("sl", false);
        Field so = field("so", false);
        Field sint = field("sint", false);
        Field sz = field("sz", false);
        Field fi = field("fi", false);
        Field ai = field("i", true);
        L3W57FieldRefusalFrames me = new L3W57FieldRefusalFrames();
        // A final field's write.
        row("final-set", () -> sfi.set(null, 5));
        row("final-setInt", () -> sfi.setInt(null, 5));
        row("final-setByte", () -> sfi.setByte(null, (byte) 5));
        row("final-setChar", () -> sfi.setChar(null, 'z'));
        row("final-acc-setInt", () -> asfi.setInt(null, 5));
        row("final-setInt-long", () -> sfl.setInt(null, 5));
        row("final-set-null", () -> sfi.set(null, null));
        row("final-set-string", () -> sfs.set(null, "y"));
        row("final-inst-set", () -> fi.set(me, 7));
        row("final-inst-setInt", () -> fi.setInt(me, 7));
        // A value of the wrong type.
        row("value-set", () -> si.set(null, "x"));
        row("value-set-null", () -> si.set(null, null));
        row("value-set-long", () -> si.set(null, 5L));
        row("value-setLong", () -> si.setLong(null, 5L));
        row("value-setBoolean", () -> si.setBoolean(null, true));
        row("value-setInt-boolean", () -> sz.setInt(null, 1));
        row("value-setInt-object", () -> so.setInt(null, 1));
        row("value-set-integer", () -> sint.set(null, "x"));
        row("value-acc-setDouble", () -> ai.setDouble(me, 1.5));
        row("value-final-setLong", () -> sfi.setLong(null, 5L));
        // A getter's illegal conversion.
        row("get-getByte", () -> si.getByte(null));
        row("get-getInt-long", () -> sl.getInt(null));
        row("get-getInt-object", () -> so.getInt(null));
        row("get-getBoolean", () -> si.getBoolean(null));
        row("get-acc-getShort", () -> ai.getShort(me));
        row("get-getChar-boolean", () -> sz.getChar(null));
        // A read-only instance field's typed setter checks its receiver in
        // `ensureObj` (a final without the override; a record's with it).
        Field rx = Rec.class.getDeclaredField("x");
        rx.setAccessible(true);
        row("ro-setInt-wrong", () -> fi.setInt("wrong", 5));
        row("ro-setByte-wrong", () -> fi.setByte("wrong", (byte) 5));
        row("ro-setLong-wrong", () -> fi.setLong("wrong", 5L));
        row("rec-acc-setInt-null", () -> rx.setInt(null, 5));
        row("rec-acc-setShort-null", () -> rx.setShort(null, (short) 5));
        row("rec-acc-setInt-wrong", () -> rx.setInt("wrong", 5));
        row("rec-acc-set-null", () -> rx.set(null, 5));
        row("final-set-again", () -> sfi.set(null, 5));
    }
}
