// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 38, lane L4: a `Class` constant of
// `StringConcatFactory.makeConcatWithConstants` whose class is NOT loaded
// when the site links (item 1 of
// docs/internal/fixed-bugs/interpreter-L4-review-of-waves-30-36-invoke-and-indy-small-divergences-FIXED-20261003.md).
// HotSpot resolves the constant (loading the class) before the factory runs,
// so an interface renders `interface X` and a missing class is the
// resolution's `NoClassDefFoundError`. Each caller is generated with the
// java.lang.classfile API: `make()` is `"k" + <recipe \u0001=\u0002>`.
//
// Before wave 38 CratonVM rendered an unloaded interface as `class X`, and a
// missing class as `class p.Missing`. `--compatible` keeps that (by design:
// the resolution is `--jdk-only`); its `lazy-iface` line reads
// `k=class L4W38ConcatClassConstant$LazyIface` and its `missing` line
// `k=class p.Missing`.
//
// Run: javac -d out L4W38ConcatClassConstant.java
//      cratonvm --java-home <jdk25> [--nojit] -cp out L4W38ConcatClassConstant
//
// Expected HotSpot 25 output (default and -Xint):
//   lazy-iface: k=interface L4W38ConcatClassConstant$LazyIface
//   lazy-iface-again: k=interface L4W38ConcatClassConstant$LazyIface
//   lazy-class: k=class L4W38ConcatClassConstant$LazyClass
//   loaded-iface: k=interface java.lang.Runnable
//   missing: java.lang.NoClassDefFoundError: p/Missing
//   missing-again: java.lang.NoClassDefFoundError: p/Missing
//   hot-iface: 30000
//
// `hot-iface` calls one site 30 000 times, so its method is compiled. The
// compiled concat bridge rendered its constants from the constant pool
// (`class java.lang.Runnable`) before wave 38; it now declines a site with a
// `Class` constant, which keeps its trap and is answered by the interpreter.
// Positive control: CRATONVM_DBG_INDY_ALL=1 prints
// `[indy-all] jit concat bridge declined cp#N (Ljava/lang/String;)Ljava/lang/String;: trap`
// when `make` compiles (every mode, `--compatible` included).
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.DirectMethodHandleDesc;
import java.lang.constant.DynamicCallSiteDesc;
import java.lang.constant.MethodHandleDesc;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class L4W38ConcatClassConstant {
    // Never referenced by the probe's own code: loaded first by the site.
    public interface LazyIface {
    }

    public static class LazyClass {
    }

    static final ClassDesc SCF = ClassDesc.of("java.lang.invoke.StringConcatFactory");
    static final DirectMethodHandleDesc MCWC = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, SCF,
            "makeConcatWithConstants", MethodTypeDesc.of(ClassDesc.of("java.lang.invoke.CallSite"),
                    ClassDesc.of("java.lang.invoke.MethodHandles$Lookup"), ConstantDescs.CD_String,
                    ClassDesc.of("java.lang.invoke.MethodType"), ConstantDescs.CD_String,
                    ConstantDescs.CD_Object.arrayType()));

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W38ConcatClassConstant.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static final Loader LOADER = new Loader();

    static Method caller(String name, String constant) throws Exception {
        DynamicCallSiteDesc site = DynamicCallSiteDesc.of(MCWC, "concat",
                MethodTypeDesc.of(ConstantDescs.CD_String, ConstantDescs.CD_String), "\u0001=\u0002",
                ClassDesc.of(constant));
        byte[] b = ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("make", MethodTypeDesc.of(ConstantDescs.CD_String),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.ldc("k").invokedynamic(site).areturn());
        });
        return LOADER.define(name, b).getMethod("make");
    }

    static String call(Method make) {
        try {
            return String.valueOf(make.invoke(null));
        } catch (InvocationTargetException e) {
            return e.getCause().toString();
        } catch (Throwable t) {
            return "setup: " + t;
        }
    }

    public static void main(String[] args) throws Exception {
        Method lazyIface = caller("LcLazyIface", L4W38ConcatClassConstant.class.getName() + "$LazyIface");
        System.out.println("lazy-iface: " + call(lazyIface));
        System.out.println("lazy-iface-again: " + call(lazyIface));
        System.out.println("lazy-class: " + call(caller("LcLazyClass",
                L4W38ConcatClassConstant.class.getName() + "$LazyClass")));
        System.out.println("loaded-iface: " + call(caller("LcLoadedIface", "java.lang.Runnable")));
        Method missing = caller("LcMissing", "p.Missing");
        System.out.println("missing: " + call(missing));
        System.out.println("missing-again: " + call(missing));
        MethodHandle hot = MethodHandles.lookup().findStatic(
                caller("LcHotIface", "java.lang.Runnable").getDeclaringClass(), "make",
                MethodType.methodType(String.class));
        int same = 0;
        for (int i = 0; i < 30_000; i++) {
            try {
                if ("k=interface java.lang.Runnable".equals((String) hot.invokeExact())) {
                    same++;
                }
            } catch (Throwable t) {
                // counted as a mismatch
            }
        }
        System.out.println("hot-iface: " + same);
    }
}
