// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i4 wave 88, lane L4: the method-handle half of a fresh
// class's first use. `L4W87DefineClassSplitBench` row `findConstructor`
// (`lookup().findConstructor(c, ()V)` then `invoke()` on a class just
// defined) measured 742 us on CratonVM `--jdk-only` against 12.75 us on
// HotSpot. This splits it, each row on its own fresh class (the define and,
// where named, the init are untimed preparation):
//
//   find-fresh          findConstructor on a class just defined (not initialized)
//   find-inited         findConstructor on a class already initialized
//   invoke-first        the first `invoke()` of a handle found untimed (class not
//                       initialized: allocateInstance initializes it)
//   invoke-first-inited the same on an initialized class
//   invokeExact-first   the first `invokeExact()` of the handle adapted untimed
//                       with `asType(()Object)` (the adaptation is not timed)
//   asType-first        that `asType(()Object)` alone
//   invoke-second       a second `invoke()` of the same handle
//   find-warm-class     findConstructor again on a class already used (new
//                       handle, warm class: per-handle cost only)
//   findStatic-fresh    findStatic of a static `()I` on a fresh class, then
//                       `invoke()` (the method finder, not the constructor)
//   findStatic-inited   the same on an initialized class (no `<clinit>`, and
//                       the handle's form is LF_INVSTATIC from the start: no
//                       `ensureInitialized` / `updateForm` on the first call)
//   findStatic-find-only        findStatic alone on a fresh class
//   findStatic-invoke-first     the first invoke() of a handle found untimed
//                               (fresh class: initializes, then `updateForm`)
//   findStatic-warm-class       findStatic again, same arguments, on a class
//                               already found and called: the repeat-finder
//                               case the method finder mint memo serves
//                               (`CRATONVM_MH_METHOD_FINDER_MINT_MEMO`; with a
//                               JIT only under the `method-finder-mint` row of
//                               `CRATONVM_INTRINSIC_BODY_COMPILES_ROWS`)
//
// Wave 88 A/B for `CRATONVM_MH_REAL_CTOR_WIDENING_SERVE` (default off): rows
// `invoke-first-inited` and `invoke-second` (an `()Object` invoke of an
// initialized class's nullary constructor handle) drop with `=1`; every other
// row and every checksum is unchanged.
//
// Comparing the rows names the costly part: per CLASS (find-fresh vs
// find-warm-class, invoke-first vs invoke-first-inited), per HANDLE
// (invoke-first vs invoke-second), or the adaptation (asType-first).
//
// Output: "row <name> min <ns> med <ns> ns/op" and "checksum <name> <n>".
// Args: [n] (default 300).
//
// Run: javac -d . L4W88FindConstructorSplitBench.java
//      java -cp . L4W88FindConstructorSplitBench                  (HotSpot reference)
//      cratonvm --jdk-only         -cp . L4W88FindConstructorSplitBench
//      cratonvm --jdk-only --nojit -cp . L4W88FindConstructorSplitBench
//      cratonvm --compatible       -cp . L4W88FindConstructorSplitBench
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum find-fresh 300
// checksum find-inited 300
// checksum invoke-first 300
// checksum invoke-first-inited 300
// checksum invokeExact-first 300
// checksum asType-first 300
// checksum invoke-second 300
// checksum find-warm-class 300
// checksum findStatic-fresh 300
// checksum findStatic-inited 300
// checksum findStatic-find-only 300
// checksum findStatic-invoke-first 300
// checksum findStatic-warm-class 300
// HOTSPOT_EXPECTED_END
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.Arrays;
import java.util.Locale;

public class L4W88FindConstructorSplitBench {
    static final MethodTypeDesc VOID = MethodTypeDesc.of(ConstantDescs.CD_void);
    static final MethodType CTOR = MethodType.methodType(void.class);
    static final MethodType TO_OBJECT = MethodType.methodType(Object.class);
    static final MethodType INT = MethodType.methodType(int.class);

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W88FindConstructorSplitBench.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static int classes;

    /** An empty public class with a public no-arg constructor and `public static int seven()`. */
    static byte[] build(String name) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody(ConstantDescs.INIT_NAME, VOID, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME, VOID)
                            .return_());
            cb.withMethodBody("seven", MethodTypeDesc.of(ConstantDescs.CD_int),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> code.bipush(7).ireturn());
        });
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

    interface Prep {
        Object prepare(Class<?> c) throws Throwable;
    }

    interface Timed {
        /** Answers 1 when the step produced what it should. */
        int run(Class<?> c, Object prepared) throws Throwable;
    }

    static final MethodHandles.Lookup LOOKUP = MethodHandles.lookup();

    static void row(String name, int n, boolean print, boolean init, Prep prep, Timed timed) throws Throwable {
        Loader loader = new Loader();
        long[] t = new long[n];
        int ok = 0;
        for (int i = 0; i < n; i++) {
            String cls = "Fc" + (classes++);
            Class<?> c = loader.define(cls, build(cls));
            if (init) {
                Class.forName(cls, true, loader);
            }
            Object prepared = prep == null ? null : prep.prepare(c);
            long t0 = System.nanoTime();
            ok += timed.run(c, prepared);
            t[i] = System.nanoTime() - t0;
        }
        if (print) {
            System.out.printf(Locale.ROOT, "row %s min %.1f med %.1f ns/op%n", name, min(t), median(t));
            System.out.println("checksum " + name + " " + ok);
        }
    }

    public static void main(String[] args) throws Throwable {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 300;
        Prep find = c -> LOOKUP.findConstructor(c, CTOR);
        Timed findTimed = (c, p) -> LOOKUP.findConstructor(c, CTOR).type().parameterCount() == 0 ? 1 : 0;
        Timed invoke = (c, p) -> ((MethodHandle) p).invoke().getClass() == c ? 1 : 0;
        for (int pass = 0; pass < 2; pass++) {
            boolean print = pass == 1;
            int count = print ? n : 40;
            row("find-fresh", count, print, false, null, findTimed);
            row("find-inited", count, print, true, null, findTimed);
            row("invoke-first", count, print, false, find, invoke);
            row("invoke-first-inited", count, print, true, find, invoke);
            row("invokeExact-first", count, print, false,
                    c -> LOOKUP.findConstructor(c, CTOR).asType(TO_OBJECT),
                    (c, p) -> ((Object) ((MethodHandle) p).invokeExact()).getClass() == c ? 1 : 0);
            row("asType-first", count, print, false, find,
                    (c, p) -> ((MethodHandle) p).asType(TO_OBJECT).type() == TO_OBJECT ? 1 : 0);
            row("invoke-second", count, print, false, c -> {
                MethodHandle h = LOOKUP.findConstructor(c, CTOR);
                h.invoke();
                return h;
            }, invoke);
            row("find-warm-class", count, print, false, c -> {
                LOOKUP.findConstructor(c, CTOR).invoke();
                return null;
            }, findTimed);
            row("findStatic-fresh", count, print, false, null,
                    (c, p) -> (int) LOOKUP.findStatic(c, "seven", INT).invoke() == 7 ? 1 : 0);
            row("findStatic-inited", count, print, true, null,
                    (c, p) -> (int) LOOKUP.findStatic(c, "seven", INT).invoke() == 7 ? 1 : 0);
            row("findStatic-find-only", count, print, false, null,
                    (c, p) -> LOOKUP.findStatic(c, "seven", INT).type() == INT ? 1 : 0);
            row("findStatic-invoke-first", count, print, false, c -> LOOKUP.findStatic(c, "seven", INT),
                    (c, p) -> (int) ((MethodHandle) p).invoke() == 7 ? 1 : 0);
            row("findStatic-warm-class", count, print, false, c -> {
                LOOKUP.findStatic(c, "seven", INT).invoke();
                return null;
            }, (c, p) -> LOOKUP.findStatic(c, "seven", INT).type() == INT ? 1 : 0);
        }
    }
}
