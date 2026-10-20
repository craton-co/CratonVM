// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i4 wave 87, lane L4: where the ~0.7-1.4 ms CratonVM spends
// on "define a tiny class through a user loader and instantiate it"
// (`L4W87IndyLinkCostBench` `control-define`: HotSpot ~25 us) goes. The same
// tiny class (an empty public class with a public no-arg constructor, built
// with java.lang.classfile, a fresh name per measurement) is taken through
// each step separately, each step timed on its own fresh class:
//
//   define          `ClassLoader.defineClass(name, b, 0, len)` on a user loader
//                   (Java `preDefineClass` / `defineClassSourceLocation` /
//                   `postDefineClass` around the `defineClass1` native)
//   define-pd       the same with an explicit ProtectionDomain (null CodeSource)
//   init            then `Class.forName(name, true, loader)` (link + init)
//   getConstructor  then `getConstructor()`
//   newInstance-1   then the first `Constructor.newInstance()` (the reflective
//                   accessor is built here on the JDK path)
//   newInstance-2   a second `newInstance()` on the same Constructor
//   lookup-define   `MethodHandles.lookup().defineClass(bytes)` (app loader,
//                   unnamed package, the Lookup path)
//   hidden-define   `lookup().defineHiddenClass(bytes, true)`
//   findConstructor `lookup().findConstructor(c, ()V)` on a defined class,
//                   then `invoke()` (the method-handle route to a first object)
//
// Output, all on STDOUT:
//   "row <step> min <ns> med <ns> ns/op"   over N fresh classes, after a warm-up
//   "checksum <step> <value>"              deterministic
// Args: [n] (default 300).
//
// Run: javac -d . L4W87DefineClassSplitBench.java
//      java -cp . L4W87DefineClassSplitBench                  (HotSpot reference)
//      cratonvm --jdk-only         -cp . L4W87DefineClassSplitBench
//      cratonvm --jdk-only --nojit -cp . L4W87DefineClassSplitBench
//      cratonvm --compatible       -cp . L4W87DefineClassSplitBench
// For a profile of the worst row: CRATONVM_DBG_DEFINE=1 is NOT wanted (it
// prints per define); use `perf record -g` around a run with a large n.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum define 300
// checksum define-pd 300
// checksum init 300
// checksum getConstructor 300
// checksum newInstance-1 300
// checksum newInstance-2 300
// checksum lookup-define 300
// checksum hidden-define 300
// checksum findConstructor 300
// HOTSPOT_EXPECTED_END
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Constructor;
import java.security.ProtectionDomain;
import java.util.Arrays;
import java.util.Locale;

public class L4W87DefineClassSplitBench {
    static final MethodTypeDesc VOID = MethodTypeDesc.of(ConstantDescs.CD_void);

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W87DefineClassSplitBench.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }

        Class<?> definePd(String name, byte[] b, ProtectionDomain pd) {
            return defineClass(name, b, 0, b.length, pd);
        }
    }

    static int classes;

    static byte[] build(String name) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody(ConstantDescs.INIT_NAME, VOID, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME, VOID)
                            .return_());
        });
    }

    static String next() {
        return "Dc" + (classes++);
    }

    interface Step {
        /** Answers 1 when the step produced what it should. */
        int run(Loader loader, String name, byte[] bytes) throws Throwable;
    }

    static double median(long[] xs) {
        long[] c = xs.clone();
        Arrays.sort(c);
        int m = c.length / 2;
        return c.length % 2 == 1 ? c[m] : (c[m - 1] + c[m]) / 2.0;
    }

    static double min(long[] xs) {
        long best = Long.MAX_VALUE;
        for (long x : xs) best = Math.min(best, x);
        return best;
    }

    /**
     * Runs `step` on n fresh classes. `prepare` takes each class to the state
     * the step starts from (untimed); the step's own call is timed.
     */
    static void row(String name, int n, boolean print, Step prepare, Step timed) throws Throwable {
        Loader loader = new Loader();
        long[] t = new long[n];
        int ok = 0;
        for (int i = 0; i < n; i++) {
            String cls = next();
            byte[] bytes = build(cls);
            if (prepare != null) {
                prepare.run(loader, cls, bytes);
            }
            long t0 = System.nanoTime();
            ok += timed.run(loader, cls, bytes);
            t[i] = System.nanoTime() - t0;
        }
        if (print) {
            System.out.printf(Locale.ROOT, "row %s min %.1f med %.1f ns/op%n", name, min(t), median(t));
            System.out.println("checksum " + name + " " + ok);
        }
    }

    // Per-class state handed from a prepare step to its timed step.
    static Class<?> current;
    static Constructor<?> currentCtor;

    public static void main(String[] args) throws Throwable {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 300;
        ProtectionDomain pd = new ProtectionDomain(null, null);
        MethodHandles.Lookup lookup = MethodHandles.lookup();

        Step define = (l, c, b) -> {
            current = l.define(c, b);
            return current.getName().equals(c) ? 1 : 0;
        };
        Step definePd = (l, c, b) -> {
            current = l.definePd(c, b, pd);
            return current.getName().equals(c) ? 1 : 0;
        };
        Step init = (l, c, b) -> Class.forName(c, true, l) == current ? 1 : 0;
        Step getCtor = (l, c, b) -> {
            currentCtor = current.getConstructor();
            return currentCtor.getParameterCount() == 0 ? 1 : 0;
        };
        Step newInstance = (l, c, b) -> currentCtor.newInstance().getClass() == current ? 1 : 0;
        Step lookupDefine = (l, c, b) -> lookup.defineClass(b).getName().equals(c) ? 1 : 0;
        Step hiddenDefine = (l, c, b) -> lookup.defineHiddenClass(b, true).lookupClass().isHidden() ? 1 : 0;
        Step findCtor = (l, c, b) -> {
            MethodHandle h = lookup.findConstructor(current, MethodType.methodType(void.class));
            return h.invoke().getClass() == current ? 1 : 0;
        };

        for (int pass = 0; pass < 2; pass++) {
            boolean print = pass == 1;
            int count = print ? n : 40;
            row("define", count, print, null, define);
            row("define-pd", count, print, null, definePd);
            row("init", count, print, define, init);
            row("getConstructor", count, print, (l, c, b) -> {
                define.run(l, c, b);
                return init.run(l, c, b);
            }, getCtor);
            row("newInstance-1", count, print, (l, c, b) -> {
                define.run(l, c, b);
                return getCtor.run(l, c, b);
            }, newInstance);
            row("newInstance-2", count, print, (l, c, b) -> {
                define.run(l, c, b);
                getCtor.run(l, c, b);
                return newInstance.run(l, c, b);
            }, newInstance);
            row("lookup-define", count, print, null, lookupDefine);
            row("hidden-define", count, print, null, hiddenDefine);
            row("findConstructor", count, print, define, findCtor);
        }
    }
}
