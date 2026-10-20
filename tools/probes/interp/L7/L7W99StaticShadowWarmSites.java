// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 99, lane L7: the warm (compiled) doors of a call
// site whose reference names a method only a superinterface declares STATIC.
// Static interface methods are not inherited (JVMS 5.4.3.3 / 5.4.3.4), so the
// SITE's reference does not resolve and HotSpot's NoSuchMethodError names the
// class the site named, never the receiver's class, on every call.
// Follow-up of i98-L7 (row s4 of L7W98StaticShadowDispatch: one compiled call
// named the receiver HC1).
//
//   HI   interface: static int m() = 1
//   HJ   interface extends HI (declares nothing)
//   HC1  class implements HJ;  HC2 extends HC1;  HC3 implements HJ
//   HD   the callers:
//     v1   invokevirtual   HC1.m   on an HC1
//     v2   invokevirtual   HC2.m   on an HC2
//     i1   invokeinterface HJ.m    on an HC1          (monomorphic)
//     ip   invokeinterface HJ.m    on HC1/HC2/HC3     (polymorphic receivers)
//
// Each row prints the distinct answers of 20000 calls (the JIT arm compiles
// the callers and goes through its inline-cache and by-name doors).
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] L7W99StaticShadowWarmSites
// Expected (HotSpot 25):
//   v1: [java.lang.NoSuchMethodError: 'int HC1.m()']
//   v2: [java.lang.NoSuchMethodError: 'int HC2.m()']
//   i1: [java.lang.NoSuchMethodError: 'int HJ.m()']
//   ip: [java.lang.NoSuchMethodError: 'int HJ.m()']

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

public class L7W99StaticShadowWarmSites {
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

    static String run(Method m, Object arg) {
        try {
            return String.valueOf(m.invoke(null, arg));
        } catch (InvocationTargetException e) {
            Throwable c = e.getCause();
            return c.getClass().getName() + ": " + c.getMessage();
        } catch (Throwable t) {
            return "probe failure " + t;
        }
    }

    public static void main(String[] args) throws Throwable {
        int iface = ClassFile.ACC_PUBLIC | ClassFile.ACC_INTERFACE | ClassFile.ACC_ABSTRACT;
        int cls = ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER;
        ClassDesc obj = ConstantDescs.CD_Object;
        ClassDesc[] none = {};
        ClassDesc hi = cd("HI"), hj = cd("HJ"), hc1 = cd("HC1"), hc2 = cd("HC2");

        define("HI", iface, null, none,
            cb -> cb.withMethodBody("m", INT, PUB_STATIC, code -> code.iconst_1().ireturn()));
        define("HJ", iface, null, new ClassDesc[] {hi}, cb -> { });
        Class<?> c1 = define("HC1", cls, obj, new ClassDesc[] {hj}, cb -> ctor(cb, obj));
        Class<?> c2 = define("HC2", cls, hc1, none, cb -> ctor(cb, hc1));
        Class<?> c3 = define("HC3", cls, obj, new ClassDesc[] {hj}, cb -> ctor(cb, obj));
        Class<?> hd = define("HD", cls, obj, none, cb -> {
            cb.withMethodBody("v1", MethodTypeDesc.of(ConstantDescs.CD_int, hc1), PUB_STATIC,
                code -> code.aload(0).invokevirtual(hc1, "m", INT).ireturn());
            cb.withMethodBody("v2", MethodTypeDesc.of(ConstantDescs.CD_int, hc2), PUB_STATIC,
                code -> code.aload(0).invokevirtual(hc2, "m", INT).ireturn());
            cb.withMethodBody("i1", MethodTypeDesc.of(ConstantDescs.CD_int, hj), PUB_STATIC,
                code -> code.aload(0).invokeinterface(hj, "m", INT).ireturn());
            cb.withMethodBody("ip", MethodTypeDesc.of(ConstantDescs.CD_int, hj), PUB_STATIC,
                code -> code.aload(0).invokeinterface(hj, "m", INT).ireturn());
        });
        Object o1 = c1.getConstructor().newInstance();
        Object o2 = c2.getConstructor().newInstance();
        Object o3 = c3.getConstructor().newInstance();
        String[] rows = {"v1", "v2", "i1", "ip"};
        Object[][] recv = {{o1}, {o2}, {o1}, {o1, o2, o3}};
        for (int i = 0; i < rows.length; i++) {
            Method m = null;
            for (Method cand : hd.getMethods()) if (cand.getName().equals(rows[i])) m = cand;
            Set<String> seen = new TreeSet<>();
            for (int k = 0; k < 20000; k++) seen.add(run(m, recv[i][k % recv[i].length]));
            System.out.println(rows[i] + ": " + seen);
        }
    }
}
