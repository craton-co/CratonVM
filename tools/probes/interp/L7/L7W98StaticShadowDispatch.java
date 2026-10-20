// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 98, lane L7: static and private declarations that
// method RESOLUTION finds but SELECTION must skip, and static interface
// methods, which are never inherited (JVMS 5.4.3.3 / 5.4.3.4 / 5.4.6).
// Hand-assembled with java.lang.classfile (separate-compilation shapes javac
// refuses):
//
//   HI      interface: default int d() = 3, static int m() = 1
//   HJ      interface extends HI (declares nothing)
//   HSs     class: public static int d() = 7
//   HSp     class: private int d() = 8
//   HC1     class implements HJ
//   HC7     extends HSs implements HI
//   HC9     extends HSp implements HI
//   HC13    implements HI, declares public static int d() = 13
//   HD      the callers, one invoke each:
//     s1  invokestatic    HC1.m  (Methodref)          static interface method is not inherited
//     s2  invokestatic    HJ.m   (InterfaceMethodref) nor by a subinterface
//     s3  invokevirtual   HC1.m
//     s4  invokeinterface HJ.m
//     s7  invokeinterface HI.d on an HC7    selection skips the superclass's static d
//     s8  invokevirtual   HC7.d             resolution finds HSs.d: static
//     s9  invokeinterface HI.d on an HC9    selection skips the superclass's private d
//     s10 invokevirtual   HC9.d             resolution finds HSp.d: private, no access
//     s13 invokeinterface HI.d on an HC13   the receiver's own static d is skipped
//     s14 invokevirtual   HC13.d            resolution finds HC13.d: static
//
// Each row prints its first two answers, then "warm" lists every distinct
// answer of 3000 more calls (the JIT arm compiles the callers).
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] L7W98StaticShadowDispatch
// The HotSpot 25 run is the reference. Expected by JVMS 5.4.6 (s13 is the row
// to check HotSpot itself on):
//   s1/s2/s3/s4: java.lang.NoSuchMethodError
//   s7: 3   s8: java.lang.IncompatibleClassChangeError (Expecting non-static method 'int HSs.d()')
//   s9: 3   s10: java.lang.IllegalAccessError
//   s13: 3  s14: java.lang.IncompatibleClassChangeError (Expecting non-static method 'int HC13.d()')

import java.lang.classfile.ClassBuilder;
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.Set;
import java.util.TreeSet;
import java.util.function.Consumer;

public class L7W98StaticShadowDispatch {
    static final MethodTypeDesc INT = MethodTypeDesc.of(ConstantDescs.CD_int);
    static final MethodTypeDesc VOID = MethodTypeDesc.of(ConstantDescs.CD_void);
    static final int PUB = ClassFile.ACC_PUBLIC;
    static final int PUB_STATIC = ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC;

    static ClassDesc cd(String n) { return ClassDesc.of(n); }

    static Class<?> define(String name, int flags, ClassDesc sup, ClassDesc[] ifaces,
                           Consumer<ClassBuilder> body) throws IllegalAccessException {
        byte[] bytes = ClassFile.of().build(cd(name), cb -> {
            cb.withFlags(flags);
            if (sup != null) cb.withSuperclass(sup);
            if (ifaces.length > 0) cb.withInterfaceSymbols(ifaces);
            body.accept(cb);
        });
        return MethodHandles.lookup().defineClass(bytes);
    }

    static void ctor(ClassBuilder cb, ClassDesc sup) {
        cb.withMethodBody("<init>", VOID, PUB,
            code -> code.aload(0).invokespecial(sup, "<init>", VOID).return_());
    }

    static void constant(ClassBuilder cb, String name, int flags, int value) {
        cb.withMethodBody(name, INT, flags, code -> code.ldc(Integer.valueOf(value)).ireturn());
    }

    static String run(Method m, Object arg) {
        try {
            Object r = m.getParameterCount() == 0 ? m.invoke(null) : m.invoke(null, arg);
            return String.valueOf(r);
        } catch (InvocationTargetException e) {
            Throwable c = e.getCause();
            String msg = c.getMessage();
            // HotSpot appends " (HD and HSp are in unnamed module of loader 'app')"
            // to an access error; the row keeps only the part before it.
            if (msg != null && msg.indexOf(" (") > 0) msg = msg.substring(0, msg.indexOf(" ("));
            return c.getClass().getName() + ": " + msg;
        } catch (Throwable t) {
            return "probe failure " + t;
        }
    }

    public static void main(String[] args) throws Throwable {
        int iface = ClassFile.ACC_PUBLIC | ClassFile.ACC_INTERFACE | ClassFile.ACC_ABSTRACT;
        int cls = ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER;
        ClassDesc obj = ConstantDescs.CD_Object;
        ClassDesc[] none = {};
        ClassDesc hi = cd("HI"), hj = cd("HJ"), hss = cd("HSs"), hsp = cd("HSp");
        ClassDesc hc1 = cd("HC1"), hc7 = cd("HC7"), hc9 = cd("HC9"), hc13 = cd("HC13");

        define("HI", iface, null, none, cb -> {
            constant(cb, "d", PUB, 3);
            constant(cb, "m", PUB_STATIC, 1);
        });
        define("HJ", iface, null, new ClassDesc[] {hi}, cb -> { });
        define("HSs", cls, obj, none, cb -> { ctor(cb, obj); constant(cb, "d", PUB_STATIC, 7); });
        define("HSp", cls, obj, none, cb -> { ctor(cb, obj); constant(cb, "d", ClassFile.ACC_PRIVATE, 8); });
        Class<?> c1 = define("HC1", cls, obj, new ClassDesc[] {hj}, cb -> ctor(cb, obj));
        Class<?> c7 = define("HC7", cls, hss, new ClassDesc[] {hi}, cb -> ctor(cb, hss));
        Class<?> c9 = define("HC9", cls, hsp, new ClassDesc[] {hi}, cb -> ctor(cb, hsp));
        Class<?> c13 = define("HC13", cls, obj, new ClassDesc[] {hi}, cb -> {
            ctor(cb, obj);
            constant(cb, "d", PUB_STATIC, 13);
        });
        Class<?> hd = define("HD", cls, obj, none, cb -> {
            cb.withMethodBody("s1", INT, PUB_STATIC, code -> code.invokestatic(hc1, "m", INT, false).ireturn());
            cb.withMethodBody("s2", INT, PUB_STATIC, code -> code.invokestatic(hj, "m", INT, true).ireturn());
            cb.withMethodBody("s3", MethodTypeDesc.of(ConstantDescs.CD_int, hc1), PUB_STATIC,
                code -> code.aload(0).invokevirtual(hc1, "m", INT).ireturn());
            cb.withMethodBody("s4", MethodTypeDesc.of(ConstantDescs.CD_int, hj), PUB_STATIC,
                code -> code.aload(0).invokeinterface(hj, "m", INT).ireturn());
            cb.withMethodBody("s7", MethodTypeDesc.of(ConstantDescs.CD_int, hi), PUB_STATIC,
                code -> code.aload(0).invokeinterface(hi, "d", INT).ireturn());
            cb.withMethodBody("s8", MethodTypeDesc.of(ConstantDescs.CD_int, hc7), PUB_STATIC,
                code -> code.aload(0).invokevirtual(hc7, "d", INT).ireturn());
            cb.withMethodBody("s9", MethodTypeDesc.of(ConstantDescs.CD_int, hi), PUB_STATIC,
                code -> code.aload(0).invokeinterface(hi, "d", INT).ireturn());
            cb.withMethodBody("s10", MethodTypeDesc.of(ConstantDescs.CD_int, hc9), PUB_STATIC,
                code -> code.aload(0).invokevirtual(hc9, "d", INT).ireturn());
            cb.withMethodBody("s13", MethodTypeDesc.of(ConstantDescs.CD_int, hi), PUB_STATIC,
                code -> code.aload(0).invokeinterface(hi, "d", INT).ireturn());
            cb.withMethodBody("s14", MethodTypeDesc.of(ConstantDescs.CD_int, hc13), PUB_STATIC,
                code -> code.aload(0).invokevirtual(hc13, "d", INT).ireturn());
        });
        Object o1 = c1.getConstructor().newInstance();
        Object o7 = c7.getConstructor().newInstance();
        Object o9 = c9.getConstructor().newInstance();
        Object o13 = c13.getConstructor().newInstance();
        String[] rows = {"s1", "s2", "s3", "s4", "s7", "s8", "s9", "s10", "s13", "s14"};
        Object[] recv = {null, null, o1, o1, o7, o7, o9, o9, o13, o13};
        for (int i = 0; i < rows.length; i++) {
            Method m = null;
            for (Method cand : hd.getMethods()) if (cand.getName().equals(rows[i])) m = cand;
            String a = run(m, recv[i]);
            String b = run(m, recv[i]);
            Set<String> warm = new TreeSet<>();
            for (int k = 0; k < 3000; k++) warm.add(run(m, recv[i]));
            System.out.println(rows[i] + ": " + a + " | " + b + " | warm " + warm);
        }
    }
}
