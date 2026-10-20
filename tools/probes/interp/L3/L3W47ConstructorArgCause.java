// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 47, lane L3 -- the CAUSE of the
// `IllegalArgumentException("argument type mismatch")` that
// `Constructor.newInstance` raises for an argument of the wrong type (item 2
// of docs/internal/fixed-bugs/interpreter-L3-a-reflective-argument-checks-message-and-cause-differ-from-hotspots-FIXED-20261011.md).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, measured; the same with -Xint)
// c-string-to-int: java.lang.IllegalArgumentException: argument type mismatch | cause=java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.Number (java.lang.String and java.lang.Number are in module java.base of loader 'bootstrap') | cause-trace=sun.invoke.util.ValueConversions.primitiveConversion:247,sun.invoke.util.ValueConversions.unboxInteger:81,jdk.internal.reflect.DirectConstructorHandleAccessor.newInstance:62,java.lang.reflect.Constructor.newInstanceWithCaller:499,java.lang.reflect.Constructor.newInstance:483,P.call
// c-long-to-int: java.lang.IllegalArgumentException: argument type mismatch | cause=java.lang.ClassCastException: Cannot cast java.lang.Long to java.lang.Integer | cause-trace=java.lang.Class.cast:3492,sun.invoke.util.ValueConversions.primitiveConversion:252,sun.invoke.util.ValueConversions.unboxInteger:81,jdk.internal.reflect.DirectConstructorHandleAccessor.newInstance:62,java.lang.reflect.Constructor.newInstanceWithCaller:499,java.lang.reflect.Constructor.newInstance:483,P.call
// c-double-to-long: java.lang.IllegalArgumentException: argument type mismatch | cause=java.lang.ClassCastException: Cannot cast java.lang.Double to java.lang.Long | cause-trace=java.lang.Class.cast:3492,sun.invoke.util.ValueConversions.primitiveConversion:252,sun.invoke.util.ValueConversions.unboxLong:126,jdk.internal.reflect.DirectConstructorHandleAccessor.newInstance:62,java.lang.reflect.Constructor.newInstanceWithCaller:499,java.lang.reflect.Constructor.newInstance:483,P.call
// c-integer-to-long: no exception: true
// c-integer-to-string: java.lang.IllegalArgumentException: argument type mismatch | cause=java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.String | cause-trace=java.lang.Class.cast:3492,jdk.internal.reflect.DirectConstructorHandleAccessor.newInstance:62,java.lang.reflect.Constructor.newInstanceWithCaller:499,java.lang.reflect.Constructor.newInstance:483,P.call
// c-string-to-number: java.lang.IllegalArgumentException: argument type mismatch | cause=java.lang.ClassCastException: Cannot cast java.lang.String to java.lang.Number | cause-trace=java.lang.Class.cast:3492,jdk.internal.reflect.DirectConstructorHandleAccessor.newInstance:62,java.lang.reflect.Constructor.newInstanceWithCaller:499,java.lang.reflect.Constructor.newInstance:483,P.call
// c-string-to-runnable: java.lang.IllegalArgumentException: argument type mismatch | cause=java.lang.ClassCastException: Cannot cast java.lang.String to java.lang.Runnable | cause-trace=java.lang.Class.cast:3492,jdk.internal.reflect.DirectConstructorHandleAccessor.newInstance:62,java.lang.reflect.Constructor.newInstanceWithCaller:499,java.lang.reflect.Constructor.newInstance:483,P.call
// c-integer-to-char: java.lang.IllegalArgumentException: argument type mismatch | cause=java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.Character | cause-trace=java.lang.Class.cast:3492,sun.invoke.util.ValueConversions.primitiveConversion:252,sun.invoke.util.ValueConversions.unboxCharacter:117,jdk.internal.reflect.DirectConstructorHandleAccessor.newInstance:62,java.lang.reflect.Constructor.newInstanceWithCaller:499,java.lang.reflect.Constructor.newInstance:483,P.call
// c-string-to-boolean: java.lang.IllegalArgumentException: argument type mismatch | cause=java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.Number (java.lang.String and java.lang.Number are in module java.base of loader 'bootstrap') | cause-trace=sun.invoke.util.ValueConversions.primitiveConversion:247,sun.invoke.util.ValueConversions.unboxBoolean:108,jdk.internal.reflect.DirectConstructorHandleAccessor.newInstance:62,java.lang.reflect.Constructor.newInstanceWithCaller:499,java.lang.reflect.Constructor.newInstance:483,P.call
// c-integer-to-byte: java.lang.IllegalArgumentException: argument type mismatch | cause=java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.Byte | cause-trace=java.lang.Class.cast:3492,sun.invoke.util.ValueConversions.primitiveConversion:252,sun.invoke.util.ValueConversions.unboxByte:90,jdk.internal.reflect.DirectConstructorHandleAccessor.newInstance:62,java.lang.reflect.Constructor.newInstanceWithCaller:499,java.lang.reflect.Constructor.newInstance:483,P.call
// c-string-to-short: java.lang.IllegalArgumentException: argument type mismatch | cause=java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.Number (java.lang.String and java.lang.Number are in module java.base of loader 'bootstrap') | cause-trace=sun.invoke.util.ValueConversions.primitiveConversion:247,sun.invoke.util.ValueConversions.unboxShort:99,jdk.internal.reflect.DirectConstructorHandleAccessor.newInstance:62,java.lang.reflect.Constructor.newInstanceWithCaller:499,java.lang.reflect.Constructor.newInstance:483,P.call
// c-double-to-float: java.lang.IllegalArgumentException: argument type mismatch | cause=java.lang.ClassCastException: Cannot cast java.lang.Double to java.lang.Float | cause-trace=java.lang.Class.cast:3492,sun.invoke.util.ValueConversions.primitiveConversion:252,sun.invoke.util.ValueConversions.unboxFloat:135,jdk.internal.reflect.DirectConstructorHandleAccessor.newInstance:62,java.lang.reflect.Constructor.newInstanceWithCaller:499,java.lang.reflect.Constructor.newInstance:483,P.call
// c-string-to-double: java.lang.IllegalArgumentException: argument type mismatch | cause=java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.Number (java.lang.String and java.lang.Number are in module java.base of loader 'bootstrap') | cause-trace=sun.invoke.util.ValueConversions.primitiveConversion:247,sun.invoke.util.ValueConversions.unboxDouble:144,jdk.internal.reflect.DirectConstructorHandleAccessor.newInstance:62,java.lang.reflect.Constructor.newInstanceWithCaller:499,java.lang.reflect.Constructor.newInstance:483,P.call
// c-long-to-float: no exception: true
// c-biginteger-to-int: java.lang.IllegalArgumentException: argument type mismatch | cause=java.lang.ClassCastException: Cannot cast java.math.BigInteger to java.lang.Integer | cause-trace=java.lang.Class.cast:3492,sun.invoke.util.ValueConversions.primitiveConversion:252,sun.invoke.util.ValueConversions.unboxInteger:81,jdk.internal.reflect.DirectConstructorHandleAccessor.newInstance:62,java.lang.reflect.Constructor.newInstanceWithCaller:499,java.lang.reflect.Constructor.newInstance:483,P.call
// c-boolean-to-int: java.lang.IllegalArgumentException: argument type mismatch | cause=java.lang.ClassCastException: Cannot cast java.lang.Boolean to java.lang.Integer | cause-trace=java.lang.Class.cast:3492,sun.invoke.util.ValueConversions.primitiveConversion:252,sun.invoke.util.ValueConversions.unboxInteger:81,jdk.internal.reflect.DirectConstructorHandleAccessor.newInstance:62,java.lang.reflect.Constructor.newInstanceWithCaller:499,java.lang.reflect.Constructor.newInstance:483,P.call
// c-character-to-int: no exception: true
// c-probe-object-to-int: java.lang.IllegalArgumentException: argument type mismatch | cause=java.lang.ClassCastException: class P$MTwo cannot be cast to class java.lang.Number (P$MTwo is in unnamed module of loader 'app'; java.lang.Number is in module java.base of loader 'bootstrap') | cause-trace=sun.invoke.util.ValueConversions.primitiveConversion:247,sun.invoke.util.ValueConversions.unboxInteger:81,jdk.internal.reflect.DirectConstructorHandleAccessor.newInstance:62,java.lang.reflect.Constructor.newInstanceWithCaller:499,java.lang.reflect.Constructor.newInstance:483,P.call
// c-second-arg: java.lang.IllegalArgumentException: argument type mismatch | cause=java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.String | cause-trace=java.lang.Class.cast:3492,jdk.internal.reflect.DirectConstructorHandleAccessor.newInstance:62,java.lang.reflect.Constructor.newInstanceWithCaller:499,java.lang.reflect.Constructor.newInstance:483,P.call
// m-string-to-int: java.lang.IllegalArgumentException: argument type mismatch | cause=none
// m-integer-to-string: java.lang.IllegalArgumentException: argument type mismatch | cause=none
// after: ok true
// HOTSPOT_EXPECTED_END
// Measured on the lane's machine with Temurin 25.0.3+9; the wave-47 host's
// HotSpot 25.0.4+7 prints the same JDK lines (`primitiveConversion:247`, ...).
// CratonVM hard-codes none of them: each frame's line is read from the
// loaded JDK class's `LineNumberTable` at the bci of the cast or call
// (`stackwalker::argument_cast_frames`), so another JDK build answers with
// its own lines.
//
// Host fix (wave 47, second commit): the first cut picked the FIRST
// `checkcast Number` of `primitiveConversion`, the never-failing
// `x instanceof Number` arm's (line 239); the failing cast is the final
// `else`'s (247), the last cast of a local (`last_checkcast_of_a_local_to`).
//
// Each row prints the exception's `toString()`, its cause's, and the cause's
// innermost frames down to the probe's lambda (`P.call`). `Method.invoke`'s
// type arm keeps no cause (the `m-*` rows); the constructor accessor's
// (`DirectConstructorHandleAccessor.newInstance:65`) keeps the
// `ClassCastException` its method handle raised: `Class.cast` for a
// reference parameter, `ValueConversions.primitiveConversion`'s `(Number) x`
// (HotSpot's checkcast wording) for a primitive one given a non-number, and
// its `wrapperType().cast(x)` for a number that does not widen.
//
// CratonVM on the base a451e608e (read from the code): every `c-*` row whose
// HotSpot line has a cause printed `| cause=none`
// (`coerce_arg_strict` returned a plain `RuntimeError::IllegalArgumentException`).
// Since wave 47 (lane L3) `lang_class::reflective_argument_cast` classifies the
// refusal, the native builds `new IllegalArgumentException("argument type
// mismatch", new ClassCastException(<HotSpot's message>))` inside the call's
// record, and the capture lists the cast's JDK frames above the call's
// (`stackwalker::argument_cast_frames`). Every row is expected to match, in
// both modes. `c-integer-to-long`, `c-long-to-float`, `c-character-to-int`, the
// `m-*` rows and `after` matched on the base and must keep matching.
//
// Positive control: `CRATONVM_DBG_STTRACE=1` prints, for `c-string-to-int`,
// `STTRACE_DBG_REFLECT raise throwable=java/lang/ClassCastException ...
// check=Some(ArgumentCast(NotANumber('I')))` (the base names no such check).
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W47ConstructorArgCause`.
import java.lang.reflect.Constructor;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.List;

public class L3W47ConstructorArgCause {
    interface Call {
        Object run() throws Throwable;
    }

    static String top(Throwable t) {
        List<String> out = new ArrayList<>();
        for (StackTraceElement e : t.getStackTrace()) {
            String cls = e.getClassName().replace("L3W47ConstructorArgCause", "P");
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
            out = "no exception: " + call.run();
        } catch (Throwable t) {
            Throwable c = t.getCause();
            out = t + " | cause=" + (c == null ? "none" : c + " | cause-trace=" + top(c));
        }
        System.out.println(name + ": " + out.replace("L3W47ConstructorArgCause", "P"));
    }

    public static final class MInt {
        public MInt(int x) {
        }
    }

    public static final class MLong {
        public MLong(long x) {
        }
    }

    public static final class MStr {
        public MStr(String s) {
        }
    }

    public static final class MNum {
        public MNum(Number n) {
        }
    }

    public static final class MRun {
        public MRun(Runnable r) {
        }
    }

    public static final class MChar {
        public MChar(char c) {
        }
    }

    public static final class MBool {
        public MBool(boolean c) {
        }
    }

    public static final class MByte {
        public MByte(byte b) {
        }
    }

    public static final class MShort {
        public MShort(short b) {
        }
    }

    public static final class MFloat {
        public MFloat(float b) {
        }
    }

    public static final class MDouble {
        public MDouble(double b) {
        }
    }

    public static final class MTwo {
        public MTwo(int a, String b) {
        }
    }

    public static void takesStr(String s) {
    }

    public static void takesInt(int s) {
    }

    public static void main(String[] args) throws Exception {
        Constructor<MInt> cInt = MInt.class.getConstructor(int.class);
        Constructor<MLong> cLong = MLong.class.getConstructor(long.class);
        Constructor<MStr> cStr = MStr.class.getConstructor(String.class);
        Constructor<MNum> cNum = MNum.class.getConstructor(Number.class);
        Constructor<MRun> cRun = MRun.class.getConstructor(Runnable.class);
        Constructor<MChar> cChar = MChar.class.getConstructor(char.class);
        Constructor<MBool> cBool = MBool.class.getConstructor(boolean.class);
        Constructor<MTwo> cTwo = MTwo.class.getConstructor(int.class, String.class);
        Constructor<MByte> cByte = MByte.class.getConstructor(byte.class);
        Constructor<MShort> cShort = MShort.class.getConstructor(short.class);
        Constructor<MFloat> cFloat = MFloat.class.getConstructor(float.class);
        Constructor<MDouble> cDouble = MDouble.class.getConstructor(double.class);
        Method mStr = L3W47ConstructorArgCause.class.getMethod("takesStr", String.class);
        Method mInt = L3W47ConstructorArgCause.class.getMethod("takesInt", int.class);

        row("c-string-to-int", () -> cInt.newInstance("x"));
        row("c-long-to-int", () -> cInt.newInstance(5L));
        row("c-double-to-long", () -> cLong.newInstance(1.0d));
        row("c-integer-to-long", () -> cLong.newInstance(5) != null);
        row("c-integer-to-string", () -> cStr.newInstance(7));
        row("c-string-to-number", () -> cNum.newInstance("s"));
        row("c-string-to-runnable", () -> cRun.newInstance("s"));
        row("c-integer-to-char", () -> cChar.newInstance(1));
        row("c-string-to-boolean", () -> cBool.newInstance("true"));
        row("c-integer-to-byte", () -> cByte.newInstance(1));
        row("c-string-to-short", () -> cShort.newInstance("s"));
        row("c-double-to-float", () -> cFloat.newInstance(1.0d));
        row("c-string-to-double", () -> cDouble.newInstance("s"));
        row("c-long-to-float", () -> cFloat.newInstance(1L) != null);
        row("c-biginteger-to-int", () -> cInt.newInstance(java.math.BigInteger.ONE));
        row("c-boolean-to-int", () -> cInt.newInstance(Boolean.TRUE));
        row("c-character-to-int", () -> cInt.newInstance('c') != null);
        row("c-probe-object-to-int", () -> cInt.newInstance(new MTwo(1, "x")));
        row("c-second-arg", () -> cTwo.newInstance(1, 2));
        row("m-string-to-int", () -> mInt.invoke(null, "x"));
        row("m-integer-to-string", () -> mStr.invoke(null, 7));
        System.out.println("after: ok " + (cInt.newInstance(3) != null));
    }
}
