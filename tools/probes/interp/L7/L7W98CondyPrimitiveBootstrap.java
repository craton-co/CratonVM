// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 98, lane L7: a CONSTANT_Dynamic whose bootstrap
// method is declared with a PRIMITIVE return type. `BootstrapMethodInvoker`
// calls the bootstrap through `invoke` with an `Object` return, so the answer
// is boxed by the bootstrap's declared return type (`short` -> Short,
// `boolean` -> Boolean) and only then cast (reference field type) or pushed
// through the `identity(<prim>)` funnel (primitive field type), which accepts
// the same wrapper or a widening one and is a ClassCastException otherwise,
// wrapped in BootstrapMethodError.
//
// CratonVM before wave 98 converted the raw primitive straight to the field
// type: an `int` bootstrap answered a `short` constant (HotSpot: CCE Integer
// -> Short), a `long` bootstrap's answer was pushed as an `int` constant (a
// category-2 value in a category-1 slot), a `boolean` bootstrap answered an
// `int` constant, and a `short` bootstrap's answer for an Object constant was
// an Integer instead of a Short.
//
// Each row generates (java.lang.classfile) a class whose static `get()` is
// one `ldc`/`ldc2_w` of the row's dynamic constant, boxed for printing, and
// runs it twice.
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] L7W98CondyPrimitiveBootstrap
// The HotSpot 25 run is the reference. Expected by the JDK code:
//   int-as-int: 5 (Integer) | 5 (Integer)
//   int-as-short: java.lang.BootstrapMethodError <- java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.Short (both runs)
//   short-as-short: 6 (Short)        short-as-int: 6 (Integer)       byte-as-short: 7 (Short)
//   char-as-int: 65 (Integer)        char-as-char: A (Character)     bool-as-boolean: true (Boolean)
//   bool-as-int: BME <- CCE Boolean to Integer
//   long-as-long: 8 (Long)           long-as-int: BME <- CCE Long to Integer
//   long-as-float: 8.0 (Float)       long-as-double: 8.0 (Double)    float-as-double: 1.5 (Double)
//   double-as-float: BME <- CCE Double to Float
//   int-as-object: 5 (Integer)       short-as-object: 6 (Short)      bool-as-object: true (Boolean)
//   char-as-object: A (Character)    long-as-number: 8 (Long)

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

public class L7W98CondyPrimitiveBootstrap {
    public static int bI(MethodHandles.Lookup l, String n, Class<?> t) { return 5; }
    public static short bS(MethodHandles.Lookup l, String n, Class<?> t) { return 6; }
    public static byte bB(MethodHandles.Lookup l, String n, Class<?> t) { return 7; }
    public static char bC(MethodHandles.Lookup l, String n, Class<?> t) { return 'A'; }
    public static boolean bZ(MethodHandles.Lookup l, String n, Class<?> t) { return true; }
    public static long bJ(MethodHandles.Lookup l, String n, Class<?> t) { return 8L; }
    public static float bF(MethodHandles.Lookup l, String n, Class<?> t) { return 1.5f; }
    public static double bD(MethodHandles.Lookup l, String n, Class<?> t) { return 2.5; }

    static final ClassDesc SELF = ClassDesc.of("L7W98CondyPrimitiveBootstrap");
    static int seq;

    static ClassDesc prim(char c) {
        switch (c) {
            case 'I': return ConstantDescs.CD_int;
            case 'S': return ConstantDescs.CD_short;
            case 'B': return ConstantDescs.CD_byte;
            case 'C': return ConstantDescs.CD_char;
            case 'Z': return ConstantDescs.CD_boolean;
            case 'J': return ConstantDescs.CD_long;
            case 'F': return ConstantDescs.CD_float;
            default: return ConstantDescs.CD_double;
        }
    }

    static ClassDesc wrapper(char c) {
        switch (c) {
            case 'I': return ClassDesc.of("java.lang.Integer");
            case 'S': return ClassDesc.of("java.lang.Short");
            case 'B': return ClassDesc.of("java.lang.Byte");
            case 'C': return ClassDesc.of("java.lang.Character");
            case 'Z': return ClassDesc.of("java.lang.Boolean");
            case 'J': return ClassDesc.of("java.lang.Long");
            case 'F': return ClassDesc.of("java.lang.Float");
            default: return ClassDesc.of("java.lang.Double");
        }
    }

    // bsm: the bootstrap's return char; type: the constant's descriptor char,
    // or 'O' for Object, 'N' for Number.
    static void row(String label, char bsm, char type) {
        String name = "L7W98Cpb" + (seq++);
        ClassDesc ret = prim(bsm);
        DirectMethodHandleDesc bsmDesc = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, SELF,
            "b" + bsm, MethodTypeDesc.of(ret, ConstantDescs.CD_MethodHandles_Lookup, ConstantDescs.CD_String,
                ConstantDescs.CD_Class));
        ClassDesc constType = type == 'O' ? ConstantDescs.CD_Object
            : type == 'N' ? ClassDesc.of("java.lang.Number") : prim(type);
        DynamicConstantDesc<?> condy = DynamicConstantDesc.ofNamed(bsmDesc, "c", constType);
        byte[] bytes = ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("get", MethodTypeDesc.of(ConstantDescs.CD_Object),
                ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> {
                    code.ldc(condy);
                    if (type != 'O' && type != 'N') {
                        code.invokestatic(wrapper(type), "valueOf", MethodTypeDesc.of(wrapper(type), prim(type)));
                    }
                    code.areturn();
                });
        });
        String a, b;
        try {
            Method get = MethodHandles.lookup().defineClass(bytes).getMethod("get");
            a = call(get);
            b = call(get);
        } catch (Throwable t) {
            a = b = "probe failure " + t;
        }
        System.out.println(label + ": " + a + " | " + b);
    }

    static String call(Method get) {
        try {
            Object r = get.invoke(null);
            return r + " (" + r.getClass().getSimpleName() + ")";
        } catch (InvocationTargetException e) {
            Throwable c = e.getCause();
            String s = c.getClass().getName();
            if (c.getCause() != null) {
                s += " <- " + c.getCause().getClass().getName() + ": " + c.getCause().getMessage();
            } else {
                s += ": " + c.getMessage();
            }
            return s;
        } catch (Throwable t) {
            return "probe failure " + t;
        }
    }

    public static void main(String[] args) {
        row("int-as-int", 'I', 'I');
        row("int-as-short", 'I', 'S');
        row("short-as-short", 'S', 'S');
        row("short-as-int", 'S', 'I');
        row("byte-as-short", 'B', 'S');
        row("char-as-int", 'C', 'I');
        row("char-as-char", 'C', 'C');
        row("bool-as-boolean", 'Z', 'Z');
        row("bool-as-int", 'Z', 'I');
        row("long-as-long", 'J', 'J');
        row("long-as-int", 'J', 'I');
        row("long-as-float", 'J', 'F');
        row("long-as-double", 'J', 'D');
        row("float-as-double", 'F', 'D');
        row("double-as-float", 'D', 'F');
        row("int-as-object", 'I', 'O');
        row("short-as-object", 'S', 'O');
        row("bool-as-object", 'Z', 'O');
        row("char-as-object", 'C', 'O');
        row("long-as-number", 'J', 'N');
    }
}
