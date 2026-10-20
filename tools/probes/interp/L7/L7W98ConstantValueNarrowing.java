// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 98, lane L7: `ConstantValue` attribute corners,
// hand-assembled with java.lang.classfile (javac never emits them).
//
// Row "getstatic"/"reflect": a sub-int static field's ConstantValue is an
// `Integer` entry of ANY value (the format check asks only for the tag).
// HotSpot assigns it through `byte_field_put` / `short_field_put` /
// `char_field_put` / `bool_field_put` (`initialize_static_primitive_field`),
// so it is narrowed to the field's width. CratonVM's preparation stored the
// int whole, and `getstatic` does not re-narrow, so `static final byte B`
// with ConstantValue 300 read 300 (FIXED in wave 98).
//
// Rows "init": a `getstatic` of a field that has a ConstantValue still
// initializes the class (only javac's constant inlining avoids that); the
// seed is assigned before <clinit> runs, which can read and overwrite it,
// a non-final static's ConstantValue is applied too, and an instance
// field's ConstantValue is ignored.
//
// Rows "format": a ConstantValue whose tag does not fit the field type is a
// ClassFormatError on HotSpot (`ClassFileParser::verify_constantvalue`).
// These are class-format checks (lane L5); suspected, not fixed here.
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] L7W98ConstantValueNarrowing
//
// HotSpot 25 prints (the orchestrator's HotSpot run is authoritative):
//   getstatic B=44 S=4464 C=65535 C2=32768 Z2=0 Z3=1 I=300
//   reflect B=44 S=4464 C=65535 C2=32768 Z2=false Z3=true I=300
//   init before: <none>
//   init getstatic K=7 log=[clinit saw K=7 N=1]
//   init after N=2 instance=0
//   format long-with-int: java.lang.ClassFormatError: Inconsistent constant value type in class file L7W98CvBadLong
//   format object-with-string: java.lang.ClassFormatError: Bad string initial value in class file L7W98CvBadObject
//   format int-with-string: java.lang.ClassFormatError: Inconsistent constant value type in class file L7W98CvBadInt

import java.lang.classfile.ClassBuilder;
import java.lang.classfile.ClassFile;
import java.lang.classfile.attribute.ConstantValueAttribute;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandles;
import java.util.function.Consumer;

public class L7W98ConstantValueNarrowing {
    public static final StringBuilder LOG = new StringBuilder();

    public static void log(String s) {
        if (LOG.length() > 0) LOG.append(' ');
        LOG.append(s);
    }

    public static void seen(int k, int n) {
        log("clinit saw K=" + k + " N=" + n);
    }

    static final int STATIC_FINAL = ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC | ClassFile.ACC_FINAL;

    static byte[] gen(String name, Consumer<ClassBuilder> body) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            body.accept(cb);
        });
    }

    static Class<?> define(byte[] bytes) throws IllegalAccessException {
        return MethodHandles.lookup().defineClass(bytes);
    }

    public static void main(String[] args) throws Throwable {
        narrowing();
        initOrder();
        format("long-with-int", "L7W98CvBadLong", ConstantDescs.CD_long, Integer.valueOf(1));
        format("object-with-string", "L7W98CvBadObject", ConstantDescs.CD_Object, "s");
        format("int-with-string", "L7W98CvBadInt", ConstantDescs.CD_int, "s");
    }

    static void narrowing() throws Throwable {
        ClassDesc cd = ClassDesc.of("L7W98CvNarrowGen");
        String[] names = {"B", "S", "C", "C2", "Z2", "Z3", "I"};
        ClassDesc[] types = {ConstantDescs.CD_byte, ConstantDescs.CD_short, ConstantDescs.CD_char,
            ConstantDescs.CD_char, ConstantDescs.CD_boolean, ConstantDescs.CD_boolean, ConstantDescs.CD_int};
        int[] values = {300, 70000, -1, 0x18000, 2, 3, 300};
        byte[] bytes = gen("L7W98CvNarrowGen", cb -> {
            for (int i = 0; i < names.length; i++) {
                final int v = values[i];
                cb.withField(names[i], types[i], fb -> fb
                    .withFlags(STATIC_FINAL)
                    .with(ConstantValueAttribute.of(Integer.valueOf(v))));
            }
            for (int i = 0; i < names.length; i++) {
                final String n = names[i];
                final ClassDesc t = types[i];
                cb.withMethodBody("get" + n, MethodTypeDesc.of(ConstantDescs.CD_int),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.getstatic(cd, n, t).ireturn());
            }
        });
        Class<?> c = define(bytes);
        StringBuilder sb = new StringBuilder("getstatic");
        for (String n : names) {
            sb.append(' ').append(n).append('=').append(c.getMethod("get" + n).invoke(null));
        }
        System.out.println(sb);
        sb = new StringBuilder("reflect");
        sb.append(" B=").append(c.getField("B").getByte(null));
        sb.append(" S=").append(c.getField("S").getShort(null));
        sb.append(" C=").append((int) c.getField("C").getChar(null));
        sb.append(" C2=").append((int) c.getField("C2").getChar(null));
        sb.append(" Z2=").append(c.getField("Z2").getBoolean(null));
        sb.append(" Z3=").append(c.getField("Z3").getBoolean(null));
        sb.append(" I=").append(c.getField("I").getInt(null));
        System.out.println(sb);
    }

    // L7W98CvInitGen:
    //   public static final int K = 7 (ConstantValue)
    //   public static int N = 1 (ConstantValue, NOT final)
    //   public int inst (ConstantValue 42, ignored)
    //   static { log("clinit saw K=" + K + " N=" + N); N = 2; }
    // L7W98CvInitReader.k() = getstatic L7W98CvInitGen.K
    static void initOrder() throws Throwable {
        ClassDesc gen = ClassDesc.of("L7W98CvInitGen");
        ClassDesc self = ClassDesc.of("L7W98ConstantValueNarrowing");
        byte[] genBytes = gen("L7W98CvInitGen", cb -> {
            cb.withField("K", ConstantDescs.CD_int, fb -> fb.withFlags(STATIC_FINAL)
                .with(ConstantValueAttribute.of(Integer.valueOf(7))));
            cb.withField("N", ConstantDescs.CD_int, fb -> fb
                .withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC)
                .with(ConstantValueAttribute.of(Integer.valueOf(1))));
            cb.withField("inst", ConstantDescs.CD_int, fb -> fb.withFlags(ClassFile.ACC_PUBLIC)
                .with(ConstantValueAttribute.of(Integer.valueOf(42))));
            cb.withMethodBody("<init>", MethodTypeDesc.of(ConstantDescs.CD_void), ClassFile.ACC_PUBLIC,
                code -> code.aload(0)
                    .invokespecial(ConstantDescs.CD_Object, "<init>", MethodTypeDesc.of(ConstantDescs.CD_void))
                    .return_());
            cb.withMethodBody("<clinit>", MethodTypeDesc.of(ConstantDescs.CD_void), ClassFile.ACC_STATIC,
                code -> code
                    .getstatic(gen, "K", ConstantDescs.CD_int)
                    .getstatic(gen, "N", ConstantDescs.CD_int)
                    .invokestatic(self, "seen", MethodTypeDesc.of(ConstantDescs.CD_void,
                        ConstantDescs.CD_int, ConstantDescs.CD_int))
                    .iconst_2()
                    .putstatic(gen, "N", ConstantDescs.CD_int)
                    .return_());
        });
        byte[] readerBytes = gen("L7W98CvInitReader", cb ->
            cb.withMethodBody("k", MethodTypeDesc.of(ConstantDescs.CD_int),
                ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                code -> code.getstatic(gen, "K", ConstantDescs.CD_int).ireturn()));
        Class<?> g = define(genBytes);
        Class<?> r = define(readerBytes);
        System.out.println("init before: " + (LOG.length() == 0 ? "<none>" : LOG.toString()));
        Object k = r.getMethod("k").invoke(null);
        System.out.println("init getstatic K=" + k + " log=[" + LOG + "]");
        Object inst = g.getConstructor().newInstance();
        System.out.println("init after N=" + g.getField("N").getInt(null)
            + " instance=" + g.getField("inst").getInt(inst));
    }

    static void format(String row, String name, ClassDesc type, java.lang.constant.ConstantDesc value) {
        try {
            byte[] bytes = gen(name, cb -> cb.withField("F", type, fb -> fb.withFlags(STATIC_FINAL)
                .with(ConstantValueAttribute.of(value))));
            Class<?> c = define(bytes);
            System.out.println("format " + row + ": defined, F=" + c.getField("F").get(null));
        } catch (Throwable e) {
            System.out.println("format " + row + ": " + e.getClass().getName() + ": " + e.getMessage());
        }
    }
}
