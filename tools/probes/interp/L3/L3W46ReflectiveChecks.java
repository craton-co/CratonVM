// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 46, lane L3 -- the JDK frames of a reflective call
// (`Method.invoke`, `Constructor.newInstance`) in the trace of an exception one
// of the call's ARGUMENT CHECKS raises (item 1's remainder of
// docs/known-issues/interpreter/i43-L3-a-throwable-a-reflective-native-raises-lists-no-reflection-frames-20261007.md).
//
// Each row prints the innermost frames of the thrown exception down to the
// probe's calling lambda (`P.lambda$...` shortened to `P.call`), JDK frames with
// their line, as `Class.method[:line]` joined by `,`; the `*-kind` rows print
// the exception's class and message.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, measured; the same with -Xint)
// m-arg-count: jdk.internal.reflect.DirectMethodHandleAccessor.checkArgumentCount:324,jdk.internal.reflect.DirectMethodHandleAccessor.invoke:102,java.lang.reflect.Method.invoke:565,P.call
// m-arg-count-instance: jdk.internal.reflect.DirectMethodHandleAccessor.checkArgumentCount:324,jdk.internal.reflect.DirectMethodHandleAccessor.invoke:102,java.lang.reflect.Method.invoke:565,P.call
// m-arg-count-kind: java.lang.IllegalArgumentException: wrong number of arguments: 0 expected: 1
// m-type-mismatch: jdk.internal.reflect.DirectMethodHandleAccessor.invoke:108,java.lang.reflect.Method.invoke:565,P.call
// m-type-mismatch-kind: java.lang.IllegalArgumentException: argument type mismatch
// m-wrong-receiver: jdk.internal.reflect.DirectMethodHandleAccessor.checkReceiver:199,jdk.internal.reflect.DirectMethodHandleAccessor.invoke:100,java.lang.reflect.Method.invoke:565,P.call
// m-wrong-receiver-kind: java.lang.IllegalArgumentException: object of type java.lang.String is not an instance of L3W46ReflectiveChecks$Inst
// m-null-receiver: java.lang.reflect.Method.invoke:557,P.call
// m-null-receiver-kind: java.lang.NullPointerException: Cannot invoke "Object.getClass()" because "obj" is null
// m-null-receiver-accessible: jdk.internal.reflect.DirectMethodHandleAccessor.checkReceiver:197,jdk.internal.reflect.DirectMethodHandleAccessor.invoke:100,java.lang.reflect.Method.invoke:565,P.call
// m-null-to-primitive: jdk.internal.reflect.DirectMethodHandleAccessor.invoke:114,java.lang.reflect.Method.invoke:565,P.call
// m-null-to-primitive-kind: java.lang.IllegalArgumentException: java.lang.NullPointerException: Cannot invoke "java.lang.Number.intValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
// m-null-to-long-kind: java.lang.IllegalArgumentException: java.lang.NullPointerException: Cannot invoke "java.lang.Number.longValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
// m-null-to-primitive-cause: java.lang.NullPointerException sun.invoke.util.ValueConversions.unboxInteger:81,jdk.internal.reflect.DirectMethodHandleAccessor.invoke:104,java.lang.reflect.Method.invoke:565,P.call
// m-null-to-long-cause: java.lang.NullPointerException sun.invoke.util.ValueConversions.unboxLong:126,jdk.internal.reflect.DirectMethodHandleAccessor.invoke:104,java.lang.reflect.Method.invoke:565,P.call
// m-null-to-boolean-cause: java.lang.NullPointerException sun.invoke.util.ValueConversions.unboxBoolean:108,jdk.internal.reflect.DirectMethodHandleAccessor.invoke:104,java.lang.reflect.Method.invoke:565,P.call
// c-arg-count: jdk.internal.reflect.DirectConstructorHandleAccessor.newInstance:59,java.lang.reflect.Constructor.newInstanceWithCaller:499,java.lang.reflect.Constructor.newInstance:483,P.call
// c-arg-count-kind: java.lang.IllegalArgumentException: wrong number of arguments: 0 expected: 1
// c-type-mismatch: jdk.internal.reflect.DirectConstructorHandleAccessor.newInstance:65,java.lang.reflect.Constructor.newInstanceWithCaller:499,java.lang.reflect.Constructor.newInstance:483,P.call
// c-null-to-primitive: jdk.internal.reflect.DirectConstructorHandleAccessor.newInstance:70,java.lang.reflect.Constructor.newInstanceWithCaller:499,java.lang.reflect.Constructor.newInstance:483,P.call
// c-null-to-primitive-kind: java.lang.IllegalArgumentException: java.lang.NullPointerException: Cannot invoke "java.lang.Number.intValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
// c-null-to-primitive-cause: java.lang.NullPointerException sun.invoke.util.ValueConversions.unboxInteger:81,jdk.internal.reflect.DirectConstructorHandleAccessor.newInstance:62,java.lang.reflect.Constructor.newInstanceWithCaller:499,java.lang.reflect.Constructor.newInstance:483,P.call
// target-iae-wrapped: java.lang.reflect.InvocationTargetException cause=java.lang.IllegalArgumentException: from the target
// after-a-check: ok 7
// HOTSPOT_EXPECTED_END
//
// CratonVM on the base 55834015b (read from the code): the argument checks of
// `lang_class::native_method_invoke` / `native_constructor_new_instance_body`
// return a `RuntimeError` the interpreter materializes AFTER the native
// returned and left the call's record, so `m-arg-count`,
// `m-arg-count-instance`, `m-type-mismatch`, `m-wrong-receiver`,
// `m-null-receiver`, `m-null-receiver-accessible`, `c-arg-count` and
// `c-type-mismatch` print `P.call` alone. `m-null-to-primitive` already prints
// HotSpot's line (wave 45); `c-null-to-primitive` prints
// `...DirectConstructorHandleAccessor.newInstance:65,...` (wave 45 picked the
// accessor's SECOND `new IllegalArgumentException`, which is the constructor
// accessor's type-mismatch arm, not its null arm). The `*-cause` rows print the
// call's frames at their call lines with no `ValueConversions.unbox*` frame on
// top (`java.lang.NullPointerException jdk.internal.reflect.DirectMethodHandleAccessor.invoke:104,...`;
// the native runs no `ValueConversions`). The `*-kind` rows,
// `target-iae-wrapped` and `after-a-check` match on the base and must keep
// matching.
//
// Since wave 46 each check notes which check failed on the call's record
// (`NativeExceptionAccess::note_reflective_check`) and builds its throwable by
// its real constructor inside the record, and the capture lists the frames
// HotSpot lists for that check (`stackwalker::reflective_check_entries`;
// the null-to-primitive cause gets `ValueConversions.unbox<Wrapper>` on top,
// the class loaded by the note). Every row is expected to match, in both modes.
// The `*-null-to-primitive-kind` / `*-null-to-long-kind` rows printed
// `java.lang.IllegalArgumentException: argument type mismatch` on the base
// (its cause had no message); since wave 46 the cause carries HotSpot's
// helpful message and the exception's message is the cause's `toString()`.
// The constructor type mismatch's `ClassCastException` cause is not printed
// here: CratonVM gives none (see
// docs/known-issues/interpreter/i46-L3-a-reflective-argument-checks-message-and-cause-differ-from-hotspots-20261010.md).
//
// Positive control: `CRATONVM_DBG_STTRACE=1` prints, for `m-arg-count`,
// `STTRACE_DBG_REFLECT raise throwable=java/lang/IllegalArgumentException cause=Other relined=true check=Some(ArgumentCount)`
// and `STTRACE_DBG_REFLECT on-top listed=3 raise=true` (the base prints
// neither line for that row: nothing is filled inside the record).
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W46ReflectiveChecks`.
import java.lang.reflect.Constructor;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.List;

public class L3W46ReflectiveChecks {
    interface Call {
        Object run() throws Throwable;
    }

    static String shorten(String className) {
        return className.replace("L3W46ReflectiveChecks", "P");
    }

    /** Innermost frames down to the first probe lambda. */
    static String top(Throwable t) {
        List<String> out = new ArrayList<>();
        for (StackTraceElement e : t.getStackTrace()) {
            String cls = shorten(e.getClassName());
            if (cls.equals("P") && e.getMethodName().startsWith("lambda$")) {
                out.add("P.call");
                break;
            }
            String n = cls + "." + e.getMethodName();
            if (!cls.startsWith("P") && e.getLineNumber() >= 0) {
                n += ":" + e.getLineNumber();
            }
            out.add(n);
            if (out.size() > 8) {
                out.add("...");
                break;
            }
        }
        return String.join(",", out);
    }

    static void row(String name, Call call) {
        String out;
        try {
            Object r = call.run();
            out = "no exception: " + r;
        } catch (Throwable t) {
            out = top(t);
        }
        System.out.println(name + ": " + out);
    }

    static void cause(String name, Call call) {
        String out;
        try {
            Object r = call.run();
            out = "no exception: " + r;
        } catch (Throwable t) {
            out = t.getCause() == null ? "no cause" : t.getCause().getClass().getName() + " " + top(t.getCause());
        }
        System.out.println(name + ": " + out);
    }

    static void kind(String name, Call call) {
        String out;
        try {
            Object r = call.run();
            out = "no exception: " + r;
        } catch (Throwable t) {
            out = t.getClass().getName() + (t.getMessage() == null ? "" : ": " + t.getMessage());
        }
        System.out.println(name + ": " + out);
    }

    public static void takesInt(int x) {
    }

    public static void takesLong(long x) {
    }

    public static void takesBoolean(boolean x) {
    }

    public static int twice(int x) {
        return 2 * x;
    }

    public static void iaeThrower() {
        throw new IllegalArgumentException("from the target");
    }

    public static final class Inst {
        public void run() {
        }

        public void takes(int x) {
        }
    }

    public static final class Made {
        public Made(int x) {
        }
    }

    public static void main(String[] args) throws Exception {
        Method takesInt = L3W46ReflectiveChecks.class.getMethod("takesInt", int.class);
        Method takesLong = L3W46ReflectiveChecks.class.getMethod("takesLong", long.class);
        Method takesBoolean = L3W46ReflectiveChecks.class.getMethod("takesBoolean", boolean.class);
        Method twice = L3W46ReflectiveChecks.class.getMethod("twice", int.class);
        Method iaeThrower = L3W46ReflectiveChecks.class.getMethod("iaeThrower");
        Method run = Inst.class.getMethod("run");
        Method takes = Inst.class.getMethod("takes", int.class);
        Method runAccessible = Inst.class.getMethod("run");
        runAccessible.setAccessible(true);
        Constructor<Made> made = Made.class.getConstructor(int.class);

        row("m-arg-count", () -> takesInt.invoke(null));
        row("m-arg-count-instance", () -> takes.invoke(new Inst()));
        kind("m-arg-count-kind", () -> takesInt.invoke(null));
        row("m-type-mismatch", () -> takesInt.invoke(null, "not an int"));
        kind("m-type-mismatch-kind", () -> takesInt.invoke(null, "not an int"));
        row("m-wrong-receiver", () -> run.invoke("not an Inst"));
        kind("m-wrong-receiver-kind", () -> run.invoke("not an Inst"));
        row("m-null-receiver", () -> run.invoke(null));
        kind("m-null-receiver-kind", () -> run.invoke(null));
        row("m-null-receiver-accessible", () -> runAccessible.invoke(null));
        row("m-null-to-primitive", () -> takesInt.invoke(null, (Object) null));
        kind("m-null-to-primitive-kind", () -> takesInt.invoke(null, (Object) null));
        kind("m-null-to-long-kind", () -> takesLong.invoke(null, (Object) null));
        cause("m-null-to-primitive-cause", () -> takesInt.invoke(null, (Object) null));
        cause("m-null-to-long-cause", () -> takesLong.invoke(null, (Object) null));
        cause("m-null-to-boolean-cause", () -> takesBoolean.invoke(null, (Object) null));
        row("c-arg-count", () -> made.newInstance());
        kind("c-arg-count-kind", () -> made.newInstance());
        row("c-type-mismatch", () -> made.newInstance("not an int"));
        row("c-null-to-primitive", () -> made.newInstance((Object) null));
        kind("c-null-to-primitive-kind", () -> made.newInstance((Object) null));
        cause("c-null-to-primitive-cause", () -> made.newInstance((Object) null));
        try {
            iaeThrower.invoke(null);
            System.out.println("target-iae-wrapped: no exception");
        } catch (InvocationTargetException e) {
            System.out.println("target-iae-wrapped: " + e.getClass().getName() + " cause=" + e.getCause());
        }
        // A call after a failed check is served as usual.
        System.out.println("after-a-check: ok " + twice.invoke(null, 3).toString().length() * 7);
    }
}
