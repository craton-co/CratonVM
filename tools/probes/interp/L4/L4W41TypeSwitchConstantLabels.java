// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 41, lane L4 (review): `SwitchBootstraps.typeSwitch`'s
// constant labels, as the JDK's generated matcher tests them
// (`SwitchBootstraps.generateTypeSwitch`, JDK 25):
//
//   * an `Integer` label matches a `Number` target by `intValue()` and a
//     `Character` target by `charValue()`, so `5L`, `(1L << 32) + 5` and
//     `5.9` match `5`, and a `Boolean` matches no `Integer` label;
//   * a `String` label matches by `label.equals(target)`: only a `String`
//     (CratonVM's shape reader also read a `StringBuilder` whose buffer is
//     exactly full as its text: `string-label: 0 0 1` before wave 41);
//   * a `Long` / `Float` / `Double` label is refused for a reference
//     selector (`label with illegal type found`), checked as a regression row.
//
// Each row generates (java.lang.classfile) a class whose static
// `idx(Object)` is `aload_0; iconst_0; invokedynamic typeSwitch(Object,int)int`
// over the row's labels, and prints the index it answers for each target
// (the label count is the default arm).
//
// Run: javac -d out L4W41TypeSwitchConstantLabels.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W41TypeSwitchConstantLabels
//
// Expected HotSpot 25 output (default and -Xint):
//   int-label: 0 0 0 0 0 0 0 0 0 1
//   int-label-boolean: 2 2 0 1
//   string-label: 0 1 1
//   long-label: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.IllegalArgumentException: label with illegal type found: class java.lang.Long
//   float-label: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.IllegalArgumentException: label with illegal type found: class java.lang.Float
//   double-label: java.lang.BootstrapMethodError: bootstrap method initialization exception / java.lang.IllegalArgumentException: label with illegal type found: class java.lang.Double
//
// Before wave 41 CratonVM printed `int-label: 0 1 1 1 1 0 0 0 1 1` and
// `int-label-boolean: 0 1 0 1` (read from the code: only the `Integer`,
// `Short`, `Byte`, `Character` and `Boolean` boxes were read). The
// `long-`/`float-`/`double-label` rows are `--jdk-only`'s wave-38 refusal;
// `--compatible` links them (by design) and its lines for them are not
// recorded.
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.DirectMethodHandleDesc;
import java.lang.constant.DynamicCallSiteDesc;
import java.lang.constant.MethodHandleDesc;
import java.lang.constant.MethodTypeDesc;
import java.lang.reflect.Method;
import java.util.concurrent.atomic.AtomicInteger;

public class L4W41TypeSwitchConstantLabels {
    static final ClassDesc SB = ClassDesc.of("java.lang.runtime.SwitchBootstraps");
    static final DirectMethodHandleDesc TYPE_SWITCH = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC,
            SB, "typeSwitch", MethodTypeDesc.of(ClassDesc.of("java.lang.invoke.CallSite"),
                    ClassDesc.of("java.lang.invoke.MethodHandles$Lookup"), ConstantDescs.CD_String,
                    ClassDesc.of("java.lang.invoke.MethodType"), ConstantDescs.CD_Object.arrayType()));

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W41TypeSwitchConstantLabels.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static final Loader LOADER = new Loader();

    static Method switcher(String cls, ConstantDesc... labels) throws Exception {
        DynamicCallSiteDesc site = DynamicCallSiteDesc.of(TYPE_SWITCH, "typeSwitch",
                MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_Object, ConstantDescs.CD_int), labels);
        byte[] b = ClassFile.of().build(ClassDesc.of(cls), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("idx", MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_Object),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.aload(0).iconst_0().invokedynamic(site).ireturn());
        });
        return LOADER.define(cls, b).getMethod("idx", Object.class);
    }

    static String row(String cls, Object[] targets, ConstantDesc... labels) {
        try {
            Method m = switcher(cls, labels);
            StringBuilder out = new StringBuilder();
            for (Object t : targets) {
                if (out.length() > 0) {
                    out.append(' ');
                }
                try {
                    out.append(m.invoke(null, t));
                } catch (java.lang.reflect.InvocationTargetException e) {
                    Throwable c = e.getCause();
                    Throwable cc = c.getCause();
                    out.append(c.getClass().getName()).append(": ").append(c.getMessage());
                    if (cc != null) {
                        out.append(" / ").append(cc.getClass().getName()).append(": ").append(cc.getMessage());
                    }
                    break;
                }
            }
            return out.toString();
        } catch (Throwable t) {
            return "setup: " + t;
        }
    }

    public static void main(String[] args) {
        Object[] ints = {5, 5L, (1L << 32) + 5, 5.9, 5.9f, (short) 5, (byte) 5, (char) 5, new AtomicInteger(5), 6L};
        System.out.println("int-label: " + row("LoIntLabel", ints, 5));
        Object[] bools = {Boolean.TRUE, Boolean.FALSE, 1, 0};
        System.out.println("int-label-boolean: " + row("LoIntBool", bools, 1, 0));
        Object[] strings = {"abc", new StringBuilder(3).append("abc"), "abd"};
        System.out.println("string-label: " + row("LoStringLabel", strings, "abc"));
        Object[] longs = {5L, 5, 5.0};
        System.out.println("long-label: " + row("LoLongLabel", longs, 5L));
        Object[] floats = {Float.NaN, Float.intBitsToFloat(0x7F800001), 0.0f, -0.0f, 1.5f, 1.5};
        System.out.println("float-label: " + row("LoFloatLabel", floats, Float.NaN, 0.0f, 1.5f));
        Object[] doubles = {Double.NaN, Double.longBitsToDouble(0x7FF0000000000001L), -0.0, 2.5, 2.5f};
        System.out.println("double-label: " + row("LoDoubleLabel", doubles, Double.NaN, 0.0, 2.5));
    }
}
