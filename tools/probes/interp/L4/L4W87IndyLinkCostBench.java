// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i4 wave 87, lane L4: what the FIRST execution of an
// `invokedynamic` costs -- the link (bootstrap call, MethodType creation,
// class spinning / proxy registration, call-site publication) plus the first
// invoke -- for javac's two everyday shapes, each site in a FRESH class so
// every measurement is a cold link:
//
//   lambda   `LambdaMetafactory.metafactory` over a static `()V` body, the
//            site returning a `Runnable` (javac's non-capturing lambda);
//   capture  the same with one captured `int` (`(I)Runnable`);
//   concat   `StringConcatFactory.makeConcatWithConstants` "v=\1!" over `(I)`;
//   control  a method of the same shape with NO `invokedynamic` (returns a
//            constant), so `link = first - control-first` isolates the link.
//
// Each class (java.lang.classfile, one per row and index) implements
// `IntFunction<Object>`; its `apply(int)` is the site. Per class the bench
// times: defineClass + newInstance (`define`, reported), the first `apply`
// (`first`: link + first invoke), the second `apply` (`second`: linked), and
// for the lambdas the first `run()` of the returned object (`run`).
//
// Output, all on STDOUT:
//   "row <shape>-<phase> min <ns> med <ns> ns/op"   over N classes, after a
//                                                   warm-up batch (JIT runs
//                                                   included: the VM's own code
//                                                   is warm, the sites are not)
//   "call <shape>-link med <ns> ns/op"              median of first - control first
//   "call <shape>-second-indy med <ns> ns/op"       median of second - control second
//   "call <shape>-third-indy med <ns> ns/op"        median of third - control third
//                                                   (wave 93: the linked site
//                                                   alone; `<shape>-third` is a
//                                                   third apply per class)
//   "checksum <shape> <value>"                      deterministic
// Args: [n] (default 400 classes per shape).
//
// Run: javac -d . L4W87IndyLinkCostBench.java
//      java -cp . L4W87IndyLinkCostBench                  (HotSpot reference)
//      cratonvm --jdk-only        -cp . L4W87IndyLinkCostBench
//      cratonvm --jdk-only --nojit -cp . L4W87IndyLinkCostBench
//      cratonvm --compatible       -cp . L4W87IndyLinkCostBench
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum control 400
// checksum lambda 400
// checksum capture 79800
// checksum concat v=399!
// HOTSPOT_EXPECTED_END
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.DirectMethodHandleDesc;
import java.lang.constant.DynamicCallSiteDesc;
import java.lang.constant.MethodHandleDesc;
import java.lang.constant.MethodTypeDesc;
import java.util.Arrays;
import java.util.Locale;
import java.util.function.IntFunction;

public class L4W87IndyLinkCostBench {
    static final ClassDesc SELF = ClassDesc.of("L4W87IndyLinkCostBench");
    static final ClassDesc INT_FUNCTION = ClassDesc.of("java.util.function.IntFunction");
    static final ClassDesc RUNNABLE = ClassDesc.of("java.lang.Runnable");
    static final MethodTypeDesc APPLY = MethodTypeDesc.of(ConstantDescs.CD_Object, ConstantDescs.CD_int);
    static final MethodTypeDesc VOID = MethodTypeDesc.of(ConstantDescs.CD_void);
    static final DirectMethodHandleDesc METAFACTORY = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC,
            ClassDesc.of("java.lang.invoke.LambdaMetafactory"), "metafactory",
            MethodTypeDesc.of(ConstantDescs.CD_CallSite, ConstantDescs.CD_MethodHandles_Lookup,
                    ConstantDescs.CD_String, ConstantDescs.CD_MethodType, ConstantDescs.CD_MethodType,
                    ConstantDescs.CD_MethodHandle, ConstantDescs.CD_MethodType));
    static final DirectMethodHandleDesc CONCAT = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC,
            ClassDesc.of("java.lang.invoke.StringConcatFactory"), "makeConcatWithConstants",
            MethodTypeDesc.of(ConstantDescs.CD_CallSite, ConstantDescs.CD_MethodHandles_Lookup,
                    ConstantDescs.CD_String, ConstantDescs.CD_MethodType, ConstantDescs.CD_String,
                    ConstantDescs.CD_Object.arrayType()));

    static long runs;
    static long captured;

    public static void body() {
        runs++;
    }

    public static void bodyInt(int v) {
        captured += v;
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W87IndyLinkCostBench.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static int classes;

    /** A class `name` implementing IntFunction<Object> whose apply(int) is `site`. */
    static byte[] build(String name, String shape) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withInterfaceSymbols(INT_FUNCTION);
            cb.withMethodBody(ConstantDescs.INIT_NAME, VOID, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME, VOID)
                            .return_());
            cb.withMethodBody("apply", APPLY, ClassFile.ACC_PUBLIC, code -> {
                switch (shape) {
                    case "control" -> code.ldc("c").areturn();
                    case "lambda" -> code.invokedynamic(DynamicCallSiteDesc.of(METAFACTORY, "run",
                            MethodTypeDesc.of(RUNNABLE), VOID,
                            MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, SELF, "body", VOID),
                            VOID)).areturn();
                    case "capture" -> code.iload(1).invokedynamic(DynamicCallSiteDesc.of(METAFACTORY, "run",
                            MethodTypeDesc.of(RUNNABLE, ConstantDescs.CD_int), VOID,
                            MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, SELF, "bodyInt",
                                    MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_int)),
                            VOID)).areturn();
                    default -> code.iload(1).invokedynamic(DynamicCallSiteDesc.of(CONCAT, "concat",
                            MethodTypeDesc.of(ConstantDescs.CD_String, ConstantDescs.CD_int), "v=\u0001!"))
                            .areturn();
                }
            });
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

    static void report(String name, long[] xs) {
        System.out.printf(Locale.ROOT, "row %s min %.1f med %.1f ns/op%n", name, min(xs), median(xs));
    }

    /**
     * Times `n` fresh classes of `shape`; answers the per-class first-, second-
     * and third-apply times.
     */
    @SuppressWarnings("unchecked")
    static long[][] measure(String shape, int n, boolean print, String[] checksum) throws Throwable {
        Loader loader = new Loader();
        long[] define = new long[n];
        long[] first = new long[n];
        long[] second = new long[n];
        long[] third = new long[n];
        long[] run = new long[n];
        Object last = null;
        for (int i = 0; i < n; i++) {
            String name = "Lk" + shape + (classes++);
            byte[] bytes = build(name, shape);
            long t0 = System.nanoTime();
            IntFunction<Object> f = (IntFunction<Object>) loader.define(name, bytes)
                    .getConstructor().newInstance();
            long t1 = System.nanoTime();
            Object a = f.apply(i);
            long t2 = System.nanoTime();
            Object b = f.apply(i);
            long t3 = System.nanoTime();
            if (a instanceof Runnable r) {
                r.run();
            }
            long t4 = System.nanoTime();
            // Wave 93: a third call, so a cost the SECOND call still pays as a
            // fresh method's warm-up shows as second > third.
            Object c = f.apply(i);
            long t5 = System.nanoTime();
            define[i] = t1 - t0;
            first[i] = t2 - t1;
            second[i] = t3 - t2;
            run[i] = t4 - t3;
            third[i] = t5 - t4;
            last = b;
            if (c == null) {
                throw new AssertionError();
            }
        }
        if (print) {
            report(shape + "-define", define);
            report(shape + "-first", first);
            report(shape + "-second", second);
            report(shape + "-third", third);
            if (!shape.equals("control") && !shape.equals("concat")) {
                report(shape + "-run", run);
            }
            checksum[0] = String.valueOf(last);
        }
        return new long[][] {first, second, third};
    }

    public static void main(String[] args) throws Throwable {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 400;
        String[] sum = new String[1];
        // Warm the VM's own code paths (the classfile API, defineClass, the
        // bootstraps) with a throw-away batch of every shape.
        for (String shape : new String[] {"control", "lambda", "capture", "concat"}) {
            measure(shape, 50, false, sum);
        }
        runs = 0;
        captured = 0;
        long[][] control = measure("control", n, true, sum);
        System.out.println("checksum control " + n);
        double controlMed = median(control[0]);
        double controlSecond = median(control[1]);
        double controlThird = median(control[2]);
        for (String shape : new String[] {"lambda", "capture", "concat"}) {
            long[][] t = measure(shape, n, true, sum);
            System.out.printf(Locale.ROOT, "call %s-link med %.1f ns/op%n", shape, median(t[0]) - controlMed);
            // Wave 93: the LINKED site alone -- the second / third call less
            // the control's, so the bench's own interface call into a fresh
            // class's method (and the nanoTime pair) cancels out.
            System.out.printf(Locale.ROOT, "call %s-second-indy med %.1f ns/op%n", shape,
                    median(t[1]) - controlSecond);
            System.out.printf(Locale.ROOT, "call %s-third-indy med %.1f ns/op%n", shape,
                    median(t[2]) - controlThird);
            switch (shape) {
                case "lambda" -> System.out.println("checksum lambda " + runs);
                case "capture" -> System.out.println("checksum capture " + captured);
                default -> System.out.println("checksum concat " + sum[0]);
            }
        }
    }
}
