// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 45, lane L3 -- the JDK frames of a reflective call
// (`Method.invoke`, `Constructor.newInstance`) in the trace of a throwable the
// reflective native RAISES itself (item 1 of
// docs/known-issues/interpreter/i43-L3-a-throwable-a-reflective-native-raises-lists-no-reflection-frames-20261007.md).
//
// Each row prints the innermost frames of the thrown exception down to the
// probe's calling lambda (`P.lambda$...` shortened to `P.call`), JDK frames with
// their `File:line`, as `Class.method[:line]` joined by `,`.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, measured; the same with -Xint)
// ite-static: jdk.internal.reflect.DirectMethodHandleAccessor.invoke:119,java.lang.reflect.Method.invoke:565,P.call
// ite-npe: jdk.internal.reflect.DirectMethodHandleAccessor.invoke:116,java.lang.reflect.Method.invoke:565,P.call
// ite-virtual: jdk.internal.reflect.DirectMethodHandleAccessor.invoke:119,java.lang.reflect.Method.invoke:565,P.call
// ite-ctor: jdk.internal.reflect.DirectConstructorHandleAccessor.newInstance:74,java.lang.reflect.Constructor.newInstanceWithCaller:499,java.lang.reflect.Constructor.newInstance:483,P.call
// ite-cause-kept: cause=java.lang.IllegalStateException: from the target
// iae-arg-count: jdk.internal.reflect.DirectMethodHandleAccessor.checkArgumentCount:324,jdk.internal.reflect.DirectMethodHandleAccessor.invoke:102,java.lang.reflect.Method.invoke:565,P.call
// iae-type-mismatch: jdk.internal.reflect.DirectMethodHandleAccessor.invoke:108,java.lang.reflect.Method.invoke:565,P.call
// iae-wrong-receiver: jdk.internal.reflect.DirectMethodHandleAccessor.checkReceiver:199,jdk.internal.reflect.DirectMethodHandleAccessor.invoke:100,java.lang.reflect.Method.invoke:565,P.call
// npe-null-receiver: java.lang.reflect.Method.invoke:557,P.call
// iae-null-to-primitive: jdk.internal.reflect.DirectMethodHandleAccessor.invoke:114,java.lang.reflect.Method.invoke:565,P.call
// iae-null-to-primitive-cause: sun.invoke.util.ValueConversions.unboxInteger:81,jdk.internal.reflect.DirectMethodHandleAccessor.invoke:104,java.lang.reflect.Method.invoke:565,P.main
// ctor-of-a-throwable: jdk.internal.reflect.DirectConstructorHandleAccessor.newInstance:62,java.lang.reflect.Constructor.newInstanceWithCaller:499,java.lang.reflect.Constructor.newInstance:483,P.main
// HOTSPOT_EXPECTED_END
//
// CratonVM on the base 69568bea6 (read from the code): every row lists the
// probe's caller first (`P.call` / `P.main` alone): the exception is built by
// the native with no frame of the call above it (the call's first pushed
// frame is the throwable's own constructor, which the fill-frame trim
// removes, and `stackwalker::build_slots_with_splices` listed such a call
// nowhere). Since wave 45 (read from the code; the host run decides):
//   * the `ite-*` rows, `iae-null-to-primitive` and `ctor-of-a-throwable`
//     print HotSpot's line (the call on top, the innermost frame at the
//     accessor's `throw new <class>` line picked by what the constructor was
//     handed, `stackwalker::reflective_raise_entries`, or at its call line);
//   * `iae-null-to-primitive-cause` prints
//     `jdk.internal.reflect.DirectMethodHandleAccessor.invoke:104,java.lang.reflect.Method.invoke:565,P.main`
//     (the native runs no `ValueConversions.unboxInteger`);
//   * `iae-arg-count`, `iae-type-mismatch`, `iae-wrong-receiver` and
//     `npe-null-receiver` still print `P.call` alone: they are raised after
//     the native returned (see the page's Progress (wave 45)).
// Since wave 46 (lane L3; read from the code) those four rows and
// `iae-null-to-primitive-cause` print HotSpot's lines too: the checks are
// raised inside the call (`tools/probes/interp/L3/L3W46ReflectiveChecks.java`),
// and every row is expected to match.
// Under `--compatible` an `InvocationTargetException` built through its
// synthetic stub lists the accessor at its call line (104) instead.
//
// Positive control: `CRATONVM_DBG_STTRACE=1` prints, for `ite-static`,
// `STTRACE_DBG_REFLECT raise throwable=java/lang/reflect/InvocationTargetException cause=Other relined=true`
// and `STTRACE_DBG_REFLECT on-top listed=2 raise=true` (the base prints
// neither).
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W45ReflectiveRaise`.
import java.lang.reflect.Constructor;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.List;

public class L3W45ReflectiveRaise {
    interface Call {
        Object run() throws Throwable;
    }

    static String shorten(String className) {
        return className.replace("L3W45ReflectiveRaise", "P");
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

    public static void thrower() {
        throw new IllegalStateException("from the target");
    }

    public static void npeThrower() {
        throw new NullPointerException("from the target");
    }

    public static void takesInt(int x) {
    }

    public static final class Inst {
        public void run() {
            throw new IllegalStateException("from the instance");
        }
    }

    public static final class Made {
        public Made() {
            throw new IllegalStateException("from the constructor");
        }
    }

    public static void main(String[] args) throws Exception {
        Method thrower = L3W45ReflectiveRaise.class.getMethod("thrower");
        Method npeThrower = L3W45ReflectiveRaise.class.getMethod("npeThrower");
        Method takesInt = L3W45ReflectiveRaise.class.getMethod("takesInt", int.class);
        Method run = Inst.class.getMethod("run");
        Constructor<Made> made = Made.class.getConstructor();

        row("ite-static", () -> thrower.invoke(null));
        row("ite-npe", () -> npeThrower.invoke(null));
        row("ite-virtual", () -> run.invoke(new Inst()));
        row("ite-ctor", () -> made.newInstance());
        try {
            thrower.invoke(null);
            System.out.println("ite-cause-kept: no exception");
        } catch (InvocationTargetException e) {
            System.out.println("ite-cause-kept: cause=" + e.getCause());
        }
        row("iae-arg-count", () -> takesInt.invoke(null));
        row("iae-type-mismatch", () -> takesInt.invoke(null, "not an int"));
        row("iae-wrong-receiver", () -> run.invoke("not an Inst"));
        row("npe-null-receiver", () -> run.invoke(null));
        row("iae-null-to-primitive", () -> takesInt.invoke(null, (Object) null));
        try {
            takesInt.invoke(null, (Object) null);
            System.out.println("iae-null-to-primitive-cause: no exception");
        } catch (IllegalArgumentException e) {
            System.out.println("iae-null-to-primitive-cause: "
                    + (e.getCause() == null ? "no cause" : top(e.getCause())));
        }
        // A constructor reached by `Constructor.newInstance` that is a
        // throwable's own: its trace is filled inside the call, not raised by it.
        Constructor<IllegalStateException> ise =
                IllegalStateException.class.getConstructor(String.class);
        System.out.println("ctor-of-a-throwable: " + top(ise.newInstance("made")));
    }
}
