// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i5 wave 101, lane L4: the FIRST-link cost of N distinct
// `invokedynamic` sites in ONE class (pages
// `docs/internal/fixed-bugs/interpreter-i87-L4-proposal-indy-first-link-cost-KEPT-OFF-20261224.md`,
// "Wave 101"; `CRATONVM_INDY_LINK_FILLS_THREAD_SITE`, default off).
//
// `L4W87IndyLinkCostBench` puts every site in a fresh class, so its rows also
// carry a define, a fresh loader and a class's first-lambda work (the
// shared-entry scan, the loader pin). This bench defines ONE class per shape
// holding N static methods `m<k>(int)Object`, each with exactly one site:
//
//   lambda   N distinct non-capturing lambdas (`metafactory`, each over its own
//            private static `b<k>()V`, javac's `lambda$` shape), `Runnable`;
//   concat   N distinct `makeConcatWithConstants` shapes: four call types
//            `(I)`, `(J)`, `(String)`, `(I,String)`, each recipe its own text;
//   control  the same method with NO site (`ldc "c"`).
//
// `apply(k)` (the class implements `IntFunction<Object>`) dispatches to
// `m<k>` through a tableswitch. Per site the bench times the first `apply(k)`
// (link + first invoke + the method's own first call) and the second (linked).
// `<shape>-link` = median(first) - median(control first): the link alone.
//
// Output (TIME lines are the timings; filter them before diffing):
//   TIME <shape>-first med <ns> ns/op     median over N sites
//   TIME <shape>-second med <ns> ns/op
//   TIME <shape>-link med <ns> ns/op      first minus control first
//   TIME <shape>-second-indy med <ns> ns/op
//   TIME <shape>-pair med <ns> ns/op      median of first + second per site (wave 104)
//   checksum <shape> <value>
// Args: [n] (default 400 sites per shape).
//
// Run: javac -d . L4W101IndyFirstLinkBench.java
//      java -cp . L4W101IndyFirstLinkBench                                     (HotSpot)
//      cratonvm --jdk-only [--nojit] -cp . L4W101IndyFirstLinkBench
//      CRATONVM_INDY_LINK_FILLS_THREAD_SITE=1 cratonvm --jdk-only [--nojit] -cp . L4W101IndyFirstLinkBench
//      (the same with --compatible; interleave arms, take medians;
//       CRATONVM_DBG=mh-finder-census adds the `indy-link <phase>` rows)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25; derived from the sources; TIME lines removed)
// checksum control 400
// checksum lambda 400
// checksum concat d399=399/x;
// HOTSPOT_EXPECTED_END
import java.lang.classfile.ClassFile;
import java.lang.classfile.Label;
import java.lang.classfile.instruction.SwitchCase;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.DirectMethodHandleDesc;
import java.lang.constant.DynamicCallSiteDesc;
import java.lang.constant.MethodHandleDesc;
import java.lang.constant.MethodTypeDesc;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Locale;
import java.util.function.IntFunction;

public class L4W101IndyFirstLinkBench {
    static final ClassDesc BENCH = ClassDesc.of("L4W101IndyFirstLinkBench");
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

    public static void body() {
        runs++;
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W101IndyFirstLinkBench.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static void site(java.lang.classfile.CodeBuilder code, String shape, ClassDesc self, int k) {
        switch (shape) {
            case "control" -> code.ldc("c").areturn();
            case "lambda" -> code.invokedynamic(DynamicCallSiteDesc.of(METAFACTORY, "run",
                    MethodTypeDesc.of(RUNNABLE), VOID,
                    MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, self, "b" + k, VOID),
                    VOID)).areturn();
            default -> {
                switch (k % 4) {
                    case 0 -> code.iload(0).invokedynamic(DynamicCallSiteDesc.of(CONCAT, "concat",
                            MethodTypeDesc.of(ConstantDescs.CD_String, ConstantDescs.CD_int),
                            "a" + k + "=\u0001;"));
                    case 1 -> code.iload(0).i2l().invokedynamic(DynamicCallSiteDesc.of(CONCAT, "concat",
                            MethodTypeDesc.of(ConstantDescs.CD_String, ConstantDescs.CD_long),
                            "b" + k + "=\u0001;"));
                    case 2 -> code.iload(0)
                            .invokestatic(ConstantDescs.CD_String, "valueOf",
                                    MethodTypeDesc.of(ConstantDescs.CD_String, ConstantDescs.CD_int))
                            .invokedynamic(DynamicCallSiteDesc.of(CONCAT, "concat",
                                    MethodTypeDesc.of(ConstantDescs.CD_String, ConstantDescs.CD_String),
                                    "c" + k + "=\u0001;"));
                    default -> code.iload(0).ldc("x").invokedynamic(DynamicCallSiteDesc.of(CONCAT, "concat",
                            MethodTypeDesc.of(ConstantDescs.CD_String, ConstantDescs.CD_int,
                                    ConstantDescs.CD_String),
                            "d" + k + "=\u0001/\u0001;"));
                }
                code.areturn();
            }
        }
    }

    /** One class `name` with `n` sites of `shape`, `apply(k)` calling `m<k>(k)`. */
    static byte[] build(String name, String shape, int n) {
        ClassDesc self = ClassDesc.of(name);
        return ClassFile.of().build(self, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withInterfaceSymbols(INT_FUNCTION);
            cb.withMethodBody(ConstantDescs.INIT_NAME, VOID, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME, VOID)
                            .return_());
            for (int i = 0; i < n; i++) {
                final int k = i;
                if (shape.equals("lambda")) {
                    cb.withMethodBody("b" + k, VOID, ClassFile.ACC_PRIVATE | ClassFile.ACC_STATIC,
                            code -> code.invokestatic(BENCH, "body", VOID).return_());
                }
                cb.withMethodBody("m" + k, APPLY, ClassFile.ACC_STATIC, code -> site(code, shape, self, k));
            }
            cb.withMethodBody("apply", APPLY, ClassFile.ACC_PUBLIC, code -> {
                Label dflt = code.newLabel();
                List<Label> labels = new ArrayList<>();
                List<SwitchCase> cases = new ArrayList<>();
                for (int k = 0; k < n; k++) {
                    Label l = code.newLabel();
                    labels.add(l);
                    cases.add(SwitchCase.of(k, l));
                }
                code.iload(1).tableswitch(0, n - 1, dflt, cases);
                for (int k = 0; k < n; k++) {
                    code.labelBinding(labels.get(k));
                    code.iload(1).invokestatic(self, "m" + k, APPLY).areturn();
                }
                code.labelBinding(dflt);
                code.aconst_null().areturn();
            });
        });
    }

    static double median(long[] xs) {
        long[] c = xs.clone();
        Arrays.sort(c);
        int m = c.length / 2;
        return c.length % 2 == 1 ? c[m] : (c[m - 1] + c[m]) / 2.0;
    }

    static int classes;

    /** First and second `apply(k)` times of every site of a fresh `shape` class. */
    @SuppressWarnings("unchecked")
    static long[][] measure(String shape, int n, String[] last) throws Throwable {
        String name = "Lf" + shape + (classes++);
        IntFunction<Object> f = (IntFunction<Object>) new Loader().define(name, build(name, shape, n))
                .getConstructor().newInstance();
        long[] first = new long[n];
        long[] second = new long[n];
        Object b = null;
        for (int k = 0; k < n; k++) {
            long t0 = System.nanoTime();
            Object a = f.apply(k);
            long t1 = System.nanoTime();
            b = f.apply(k);
            long t2 = System.nanoTime();
            first[k] = t1 - t0;
            second[k] = t2 - t1;
            if (a instanceof Runnable r) {
                r.run();
            }
        }
        last[0] = String.valueOf(b);
        return new long[][] {first, second};
    }

    static void time(String row, double ns) {
        System.out.printf(Locale.ROOT, "TIME %s med %.1f ns/op%n", row, ns);
    }

    public static void main(String[] args) throws Throwable {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 400;
        String[] last = new String[1];
        for (String shape : new String[] {"control", "lambda", "concat"}) {
            measure(shape, 50, last);
        }
        runs = 0;
        long[][] control = measure("control", n, last);
        double c1 = median(control[0]);
        double c2 = median(control[1]);
        time("control-first", c1);
        time("control-second", c2);
        System.out.println("checksum control " + n);
        for (String shape : new String[] {"lambda", "concat"}) {
            long[][] t = measure(shape, n, last);
            time(shape + "-first", median(t[0]));
            time(shape + "-second", median(t[1]));
            time(shape + "-link", median(t[0]) - c1);
            time(shape + "-second-indy", median(t[1]) - c2);
            // Wave 104: the switch MOVES the thread-site fill from the second
            // execution into the link, so `-link` grows by what `-second`
            // saves; judge it by the pair. (`-link` / `-second-indy` are
            // differences of medians and carry the control's noise: the w103b
            // `--nojit` control row read 0.83x with no site to act on.)
            long[] pair = new long[n];
            for (int k = 0; k < n; k++) {
                pair[k] = t[0][k] + t[1][k];
            }
            time(shape + "-pair", median(pair));
            System.out.println("checksum " + shape + " " + (shape.equals("lambda") ? String.valueOf(runs) : last[0]));
        }
    }
}
