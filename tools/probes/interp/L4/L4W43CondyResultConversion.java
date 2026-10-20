// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 43, lane L4 (review): what a `CONSTANT_Dynamic`
// does with its bootstrap's answer and failures. Each row generates (with the
// java.lang.classfile API) a class `Lo<row>` whose static `get()` is one `ldc`
// of a dynamic constant of the row's declared type, bootstrapped by
// `L4W43CondyResultConversion.bsm(Lookup, String, Class)`, which answers by
// the constant's name. `get()` runs twice; the second run shows whether the
// failure (or the value) was recorded.
//
//   str-as-integer    declared Integer, answers a String
//   null-as-int       declared int, answers null
//   long-as-int       declared int, answers a Long
//   short-as-int      declared int, answers a Short (widening)
//   char-as-int       declared int, answers a Character
//   int-as-long       declared long, answers an Integer (widening)
//   int-as-object     declared Object, answers an Integer
//   int-as-number     declared Number, answers an Integer
//   throw-error       declared Object, throws AssertionError("ae")
//   throw-linkage     declared Object, throws NoClassDefFoundError("ncdfe")
//   throw-runtime     declared Object, throws IllegalStateException("ise")
//   bool-as-int       declared int, answers a Boolean
//   int-as-short, byte-as-char, char-as-double, float-as-double: narrowing
//                     and widening pairs; null-as-*: null for each primitive
//
// Run: javac -d out L4W43CondyResultConversion.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W43CondyResultConversion
//
// Expected HotSpot 25 output (default and -Xint, measured locally):
//   str-as-integer: java.lang.BootstrapMethodError: bootstrap method initialization exception <- java.lang.ClassCastException: Cannot cast java.lang.String to java.lang.Integer | java.lang.BootstrapMethodError: bootstrap method initialization exception <- java.lang.ClassCastException: Cannot cast java.lang.String to java.lang.Integer | calls=1
//   null-as-int: java.lang.BootstrapMethodError: bootstrap method initialization exception <- java.lang.NullPointerException: Cannot invoke "java.lang.Number.intValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null | java.lang.BootstrapMethodError: bootstrap method initialization exception <- java.lang.NullPointerException: null | calls=1
//   long-as-int: java.lang.BootstrapMethodError: bootstrap method initialization exception <- java.lang.ClassCastException: Cannot cast java.lang.Long to java.lang.Integer | java.lang.BootstrapMethodError: bootstrap method initialization exception <- java.lang.ClassCastException: Cannot cast java.lang.Long to java.lang.Integer | calls=1
//   short-as-int: 6 (Integer) | 6 (Integer) | calls=1
//   char-as-int: 65 (Integer) | 65 (Integer) | calls=1
//   int-as-long: 7 (Long) | 7 (Long) | calls=1
//   int-as-object: 8 (Integer) | 8 (Integer) | calls=1
//   int-as-number: 8 (Integer) | 8 (Integer) | calls=1
//   throw-error: java.lang.AssertionError: ae | java.lang.AssertionError: ae | calls=2
//   throw-linkage: java.lang.NoClassDefFoundError: ncdfe | java.lang.NoClassDefFoundError: ncdfe | calls=1
//   throw-runtime: java.lang.BootstrapMethodError: bootstrap method initialization exception <- java.lang.IllegalStateException: ise | java.lang.BootstrapMethodError: bootstrap method initialization exception <- java.lang.IllegalStateException: ise | calls=1
//   bool-as-int: java.lang.BootstrapMethodError: bootstrap method initialization exception <- java.lang.ClassCastException: Cannot cast java.lang.Boolean to java.lang.Integer | java.lang.BootstrapMethodError: bootstrap method initialization exception <- java.lang.ClassCastException: Cannot cast java.lang.Boolean to java.lang.Integer | calls=1
//   int-as-short: java.lang.BootstrapMethodError: bootstrap method initialization exception <- java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.Short | java.lang.BootstrapMethodError: bootstrap method initialization exception <- java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.Short | calls=1
//   byte-as-char: java.lang.BootstrapMethodError: bootstrap method initialization exception <- java.lang.ClassCastException: Cannot cast java.lang.Byte to java.lang.Character | java.lang.BootstrapMethodError: bootstrap method initialization exception <- java.lang.ClassCastException: Cannot cast java.lang.Byte to java.lang.Character | calls=1
//   char-as-double: 67.0 (Double) | 67.0 (Double) | calls=1
//   float-as-double: 1.5 (Double) | 1.5 (Double) | calls=1
//   null-as-long: java.lang.BootstrapMethodError: bootstrap method initialization exception <- java.lang.NullPointerException: Cannot invoke "java.lang.Number.longValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null | java.lang.BootstrapMethodError: bootstrap method initialization exception <- java.lang.NullPointerException: null | calls=1
//   null-as-char: java.lang.BootstrapMethodError: bootstrap method initialization exception <- java.lang.NullPointerException: Cannot invoke "java.lang.Number.intValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null | java.lang.BootstrapMethodError: bootstrap method initialization exception <- java.lang.NullPointerException: null | calls=1
//   null-as-boolean: java.lang.BootstrapMethodError: bootstrap method initialization exception <- java.lang.NullPointerException: Cannot invoke "java.lang.Number.intValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null | java.lang.BootstrapMethodError: bootstrap method initialization exception <- java.lang.NullPointerException: null | calls=1
//   null-as-double: java.lang.BootstrapMethodError: bootstrap method initialization exception <- java.lang.NullPointerException: Cannot invoke "java.lang.Number.doubleValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null | java.lang.BootstrapMethodError: bootstrap method initialization exception <- java.lang.NullPointerException: null | calls=1
//   null-as-float: java.lang.BootstrapMethodError: bootstrap method initialization exception <- java.lang.NullPointerException: Cannot invoke "java.lang.Number.floatValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null | java.lang.BootstrapMethodError: bootstrap method initialization exception <- java.lang.NullPointerException: null | calls=1
//   null-as-short: java.lang.BootstrapMethodError: bootstrap method initialization exception <- java.lang.NullPointerException: Cannot invoke "java.lang.Number.shortValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null | java.lang.BootstrapMethodError: bootstrap method initialization exception <- java.lang.NullPointerException: null | calls=1
//
// The second run of a recorded failure is HotSpot's rethrow of the recorded
// error: a new error of the recorded class and message, with a new cause of
// the recorded cause's class and DETAIL message, which is null for a helpful
// NPE (its text is computed, not stored): hence `NullPointerException: null`.
// An `Error` that is not a `LinkageError` is not recorded: the bootstrap runs
// again (`throw-error`, calls=2).
//
// Read from the code, CratonVM before wave 43 (every mode,
// `constants::condy_convert_result`): the widening rows (`short-as-int`,
// `char-as-int`, `int-as-long`, `char-as-double`, `float-as-double`) were
// `ClassCastException`s (`Cannot cast java.lang.Short to java.lang.Integer`),
// and every `null-as-*` row's first cause read `CONSTANT_Dynamic of type int
// resolved to null`. Its second run still prints the first run's text (the
// record keeps the detail message CratonVM stored), fixed in wave 44:
// `docs/internal/fixed-bugs/interpreter-L4-a-recorded-condy-npe-keeps-its-message-FIXED-20261008.md`.
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.DirectMethodHandleDesc;
import java.lang.constant.DynamicConstantDesc;
import java.lang.constant.MethodHandleDesc;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class L4W43CondyResultConversion {
    static final ClassDesc SELF = ClassDesc.of("L4W43CondyResultConversion");
    static final DirectMethodHandleDesc BSM = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, SELF,
            "bsm", MethodTypeDesc.of(ConstantDescs.CD_Object, ClassDesc.of("java.lang.invoke.MethodHandles$Lookup"),
                    ConstantDescs.CD_String, ConstantDescs.CD_Class));

    static int calls;

    public static Object bsm(MethodHandles.Lookup lookup, String name, Class<?> type) {
        calls++;
        return switch (name) {
            case "str-as-integer" -> "s";
            case "null-as-int" -> null;
            case "long-as-int" -> 5L;
            case "short-as-int" -> (short) 6;
            case "char-as-int" -> 'A';
            case "int-as-long" -> 7;
            case "int-as-object", "int-as-number" -> 8;
            case "throw-error" -> throw new AssertionError("ae");
            case "throw-linkage" -> throw new NoClassDefFoundError("ncdfe");
            case "throw-runtime" -> throw new IllegalStateException("ise");
            case "bool-as-int" -> Boolean.TRUE;
            case "int-as-short" -> 9;
            case "byte-as-char" -> (byte) 66;
            case "char-as-double" -> 'C';
            case "float-as-double" -> 1.5f;
            case "null-as-long", "null-as-char", "null-as-boolean", "null-as-double", "null-as-float",
                    "null-as-short" -> null;
            default -> throw new IllegalArgumentException(name);
        };
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W43CondyResultConversion.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static final Loader LOADER = new Loader();

    static String describe(Throwable t) {
        StringBuilder sb = new StringBuilder();
        for (Throwable c = t; c != null; c = c.getCause()) {
            if (sb.length() > 0) {
                sb.append(" <- ");
            }
            sb.append(c.getClass().getName()).append(": ").append(c.getMessage());
        }
        return sb.toString();
    }

    static String row(String name, ClassDesc type) {
        String cls = "Lo" + name.replace("-", "");
        boolean wide = type.equals(ConstantDescs.CD_long);
        boolean dbl = type.equals(ConstantDescs.CD_double);
        boolean flt = type.equals(ConstantDescs.CD_float);
        byte[] b = ClassFile.of().build(ClassDesc.of(cls), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("get", MethodTypeDesc.of(type), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> {
                code.ldc(DynamicConstantDesc.ofNamed(BSM, name, type));
                if (type.isPrimitive()) {
                    if (wide) {
                        code.lreturn();
                    } else if (dbl) {
                        code.dreturn();
                    } else if (flt) {
                        code.freturn();
                    } else {
                        code.ireturn();
                    }
                } else {
                    code.areturn();
                }
            });
        });
        Method get;
        try {
            get = LOADER.define(cls, b).getMethod("get");
        } catch (Throwable t) {
            return "setup: " + t;
        }
        int before = calls;
        StringBuilder out = new StringBuilder();
        for (int i = 0; i < 2; i++) {
            if (i > 0) {
                out.append(" | ");
            }
            try {
                Object v = get.invoke(null);
                out.append(v).append(" (").append(v == null ? "null" : v.getClass().getSimpleName()).append(')');
            } catch (InvocationTargetException e) {
                out.append(describe(e.getCause()));
            } catch (Throwable t) {
                out.append("call: ").append(t);
            }
        }
        out.append(" | calls=").append(calls - before);
        return out.toString();
    }

    public static void main(String[] args) {
        ClassDesc integer = ClassDesc.of("java.lang.Integer");
        System.out.println("str-as-integer: " + row("str-as-integer", integer));
        System.out.println("null-as-int: " + row("null-as-int", ConstantDescs.CD_int));
        System.out.println("long-as-int: " + row("long-as-int", ConstantDescs.CD_int));
        System.out.println("short-as-int: " + row("short-as-int", ConstantDescs.CD_int));
        System.out.println("char-as-int: " + row("char-as-int", ConstantDescs.CD_int));
        System.out.println("int-as-long: " + row("int-as-long", ConstantDescs.CD_long));
        System.out.println("int-as-object: " + row("int-as-object", ConstantDescs.CD_Object));
        System.out.println("int-as-number: " + row("int-as-number", ClassDesc.of("java.lang.Number")));
        System.out.println("throw-error: " + row("throw-error", ConstantDescs.CD_Object));
        System.out.println("throw-linkage: " + row("throw-linkage", ConstantDescs.CD_Object));
        System.out.println("throw-runtime: " + row("throw-runtime", ConstantDescs.CD_Object));
        System.out.println("bool-as-int: " + row("bool-as-int", ConstantDescs.CD_int));
        System.out.println("int-as-short: " + row("int-as-short", ConstantDescs.CD_short));
        System.out.println("byte-as-char: " + row("byte-as-char", ConstantDescs.CD_char));
        System.out.println("char-as-double: " + row("char-as-double", ConstantDescs.CD_double));
        System.out.println("float-as-double: " + row("float-as-double", ConstantDescs.CD_double));
        System.out.println("null-as-long: " + row("null-as-long", ConstantDescs.CD_long));
        System.out.println("null-as-char: " + row("null-as-char", ConstantDescs.CD_char));
        System.out.println("null-as-boolean: " + row("null-as-boolean", ConstantDescs.CD_boolean));
        System.out.println("null-as-double: " + row("null-as-double", ConstantDescs.CD_double));
        System.out.println("null-as-float: " + row("null-as-float", ConstantDescs.CD_float));
        System.out.println("null-as-short: " + row("null-as-short", ConstantDescs.CD_short));
    }
}
