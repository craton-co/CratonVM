// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;
import java.util.Arrays;
import java.util.EnumSet;
import java.util.List;
import java.util.Optional;
import java.util.Set;
import java.util.concurrent.CountedCompleter;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.ForkJoinPool;
import java.util.concurrent.ForkJoinTask;
import java.util.concurrent.RecursiveAction;
import java.util.concurrent.RecursiveTask;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import java.util.function.Supplier;
import java.util.stream.Collectors;
import java.util.stream.IntStream;
import java.util.stream.LongStream;

/**
 * SBR wave 4, lane {@code fjp}: {@code java.util.concurrent.ForkJoinPool} and its
 * task classes, {@code java.lang.StackWalker} (and the stream its {@code walk}
 * hands the caller), {@code java.lang.Runtime} / {@code Runtime.Version}, and
 * {@code jdk.internal.misc.Signal} (reached through {@code sun.misc.Signal}).
 *
 * <p>Every row is evaluated ONCE and printed (the first-run value -- a native
 * that wins from the first call shows here), then re-evaluated inside a hot
 * loop and compared with that first value; {@code drift} counts mismatches, so
 * cold == warm is asserted rather than assumed. Diff the whole output against
 * HotSpot 25. Nothing printed depends on the CPU count, identity hashes or
 * scheduling order.
 *
 * <pre>
 *   java SbrW4Fjp                       (HotSpot oracle)
 *   cratonvm --jdk-only -cp . SbrW4Fjp
 * </pre>
 */
public class SbrW4Fjp {
    static final List<String> LABELS = new ArrayList<>();
    static final List<Supplier<String>> ROWS = new ArrayList<>();

    interface X {
        Object get() throws Throwable;
    }

    static void rowx(String label, X x) {
        LABELS.add(label);
        ROWS.add(() -> {
            try {
                return String.valueOf(x.get());
            } catch (Throwable t) {
                return "!" + t.getClass().getName() + ": " + t.getMessage();
            }
        });
    }

    static String safe(Supplier<String> s) {
        try {
            return s.get();
        } catch (Throwable t) {
            return "!" + t.getClass().getName() + ": " + t.getMessage();
        }
    }

    // ---------------------------------------------------------------- fixtures

    static final class Sum extends RecursiveTask<Long> {
        final long lo, hi;

        Sum(long lo, long hi) {
            this.lo = lo;
            this.hi = hi;
        }

        @Override
        protected Long compute() {
            if (hi - lo <= 64) {
                long s = 0;
                for (long i = lo; i < hi; i++) s += i;
                return s;
            }
            long mid = (lo + hi) >>> 1;
            Sum l = new Sum(lo, mid);
            Sum r = new Sum(mid, hi);
            l.fork();
            long rv = r.compute();
            return l.join() + rv;
        }
    }

    static final class Fill extends RecursiveAction {
        final int[] a;
        final int lo, hi;

        Fill(int[] a, int lo, int hi) {
            this.a = a;
            this.lo = lo;
            this.hi = hi;
        }

        @Override
        protected void compute() {
            if (hi - lo <= 16) {
                for (int i = lo; i < hi; i++) a[i] = i * 3;
                return;
            }
            int mid = (lo + hi) >>> 1;
            invokeAll(new Fill(a, lo, mid), new Fill(a, mid, hi));
        }
    }

    static final class Boom extends RecursiveTask<Integer> {
        @Override
        protected Integer compute() {
            throw new IllegalStateException("boom");
        }
    }

    static final class Leaf extends CountedCompleter<Void> {
        final AtomicLong acc;
        final int lo, hi;

        Leaf(CountedCompleter<?> parent, AtomicLong acc, int lo, int hi) {
            super(parent);
            this.acc = acc;
            this.lo = lo;
            this.hi = hi;
        }

        @Override
        public void compute() {
            int l = lo, h = hi;
            while (h - l > 8) {
                int mid = (l + h) >>> 1;
                addToPendingCount(1);
                new Leaf(this, acc, mid, h).fork();
                h = mid;
            }
            for (int i = l; i < h; i++) acc.addAndGet(i);
            tryComplete();
        }
    }

    static final class Where extends RecursiveTask<String> {
        final ForkJoinPool expect;

        Where(ForkJoinPool expect) {
            this.expect = expect;
        }

        @Override
        protected String compute() {
            return "in=" + ForkJoinTask.inForkJoinPool() + " poolSame=" + (ForkJoinTask.getPool() == expect);
        }
    }

    static String frames(java.util.stream.Stream<StackWalker.StackFrame> s) {
        return s.filter(f -> f.getClassName().startsWith("SbrW4Fjp"))
                .map(f -> f.getMethodName() + ":" + (f.getLineNumber() > 0))
                .limit(3)
                .collect(Collectors.joining(","));
    }

    static String deep(int n) {
        if (n == 0) {
            return StackWalker.getInstance().walk(SbrW4Fjp::frames);
        }
        return deep(n - 1);
    }

    static Class<?> callerOf() {
        return StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE).getCallerClass();
    }

    public static void main(String[] args) throws Exception {
        // ------------------------------------------------ ForkJoinPool / tasks
        rowx("fjp.commonPool same", () -> ForkJoinPool.commonPool() == ForkJoinPool.commonPool());
        rowx("fjp.common factory class", () -> ForkJoinPool.commonPool().getFactory().getClass().getName());
        rowx("fjp.common factory == default",
                () -> ForkJoinPool.commonPool().getFactory() == ForkJoinPool.defaultForkJoinWorkerThreadFactory);
        rowx("fjp.default factory class", () -> ForkJoinPool.defaultForkJoinWorkerThreadFactory.getClass().getName());
        rowx("fjp.commonParallelism >=1 && agree", () -> ForkJoinPool.getCommonPoolParallelism() >= 1
                && ForkJoinPool.getCommonPoolParallelism() == ForkJoinPool.commonPool().getParallelism());
        rowx("fjp.common invoke Sum", () -> ForkJoinPool.commonPool().invoke(new Sum(0, 10_000)));
        rowx("fjp.Sum.invoke()", () -> new Sum(0, 5_000).invoke());
        rowx("fjp.pool2 invoke Sum", () -> {
            ForkJoinPool p = new ForkJoinPool(2);
            try {
                return p.invoke(new Sum(0, 20_000)) + " par=" + p.getParallelism();
            } finally {
                p.shutdown();
            }
        });
        rowx("fjp.pool2 submit get", () -> {
            ForkJoinPool p = new ForkJoinPool(2);
            try {
                return p.submit(new Sum(0, 1000)).get(10, TimeUnit.SECONDS) + " "
                        + p.submit(() -> "callable").get() + " "
                        + p.submit(() -> {}, "runnable-result").get();
            } finally {
                p.shutdown();
            }
        });
        rowx("fjp.pool2 execute+awaitQuiescence", () -> {
            ForkJoinPool p = new ForkJoinPool(2);
            AtomicInteger ran = new AtomicInteger();
            try {
                for (int i = 0; i < 8; i++) p.execute(ran::incrementAndGet);
                boolean q = p.awaitQuiescence(10, TimeUnit.SECONDS);
                return q + " ran=" + ran.get();
            } finally {
                p.shutdown();
            }
        });
        rowx("fjp.pool2 invokeAll", () -> {
            ForkJoinPool p = new ForkJoinPool(2);
            try {
                List<java.util.concurrent.Callable<Integer>> cs = new ArrayList<>();
                for (int i = 0; i < 5; i++) {
                    int k = i;
                    cs.add(() -> k * k);
                }
                StringBuilder b = new StringBuilder();
                for (var f : p.invokeAll(cs)) b.append(f.get()).append(',');
                return b;
            } finally {
                p.shutdown();
            }
        });
        rowx("fjp.pool2 shutdown/terminated", () -> {
            ForkJoinPool p = new ForkJoinPool(2);
            p.invoke(new Sum(0, 100));
            p.shutdown();
            return p.isShutdown() + " " + p.awaitTermination(10, TimeUnit.SECONDS) + " " + p.isTerminated();
        });
        rowx("fjp.RecursiveAction invokeAll", () -> {
            int[] a = new int[1000];
            ForkJoinPool.commonPool().invoke(new Fill(a, 0, a.length));
            long s = 0;
            for (int v : a) s += v;
            return s;
        });
        rowx("fjp.CountedCompleter", () -> {
            AtomicLong acc = new AtomicLong();
            new Leaf(null, acc, 0, 1000).invoke();
            return acc.get();
        });
        rowx("fjp.exceptional join", () -> {
            Boom b = new Boom();
            try {
                b.invoke();
                return "no-throw";
            } catch (IllegalStateException e) {
                return "ISE " + e.getMessage() + " done=" + b.isDone() + " abn=" + b.isCompletedAbnormally()
                        + " ex=" + (b.getException() == null ? null : b.getException().getClass().getName());
            }
        });
        rowx("fjp.exceptional get", () -> {
            ForkJoinPool p = new ForkJoinPool(2);
            try {
                p.submit(new Boom()).get();
                return "no-throw";
            } catch (ExecutionException e) {
                return "EE cause=" + e.getCause().getClass().getName() + ": " + e.getCause().getMessage();
            } finally {
                p.shutdown();
            }
        });
        rowx("fjp.complete/completeExceptionally", () -> {
            Sum s = new Sum(0, 10);
            s.complete(42L);
            Sum t = new Sum(0, 10);
            t.completeExceptionally(new ArithmeticException("x"));
            return s.join() + " " + s.isCompletedNormally() + " " + t.isCompletedAbnormally() + " "
                    + t.getException().getClass().getName();
        });
        rowx("fjp.cancel", () -> {
            Sum s = new Sum(0, 10);
            boolean c = s.cancel(true);
            return c + " " + s.isCancelled() + " " + s.isDone() + " " + s.isCompletedAbnormally();
        });
        rowx("fjp.reinitialize", () -> {
            Sum s = new Sum(0, 100);
            long a = s.invoke();
            s.reinitialize();
            return a + " done=" + s.isDone() + " again=" + s.invoke();
        });
        rowx("fjp.quietlyInvoke/Join", () -> {
            Sum s = new Sum(0, 300);
            s.quietlyInvoke();
            s.quietlyJoin();
            return s.getRawResult() + " " + s.isCompletedNormally();
        });
        rowx("fjp.inForkJoinPool outside", () -> ForkJoinTask.inForkJoinPool() + " " + (ForkJoinTask.getPool() == null));
        rowx("fjp.inForkJoinPool inside", () -> {
            ForkJoinPool p = new ForkJoinPool(2);
            try {
                return p.invoke(new Where(p));
            } finally {
                p.shutdown();
            }
        });
        rowx("fjp.parallel stream sum", () -> LongStream.rangeClosed(1, 100_000).parallel().sum());
        rowx("fjp.parallel IntStream map/filter",
                () -> IntStream.range(0, 10_000).parallel().filter(i -> i % 3 == 0).mapToLong(i -> (long) i * i).sum());
        rowx("fjp.parallel collect", () -> IntStream.range(0, 200).parallel().boxed()
                .collect(Collectors.toList()).size());

        // ------------------------------------------------ StackWalker
        rowx("sw.getInstance()", () -> StackWalker.getInstance().getClass().getName());
        rowx("sw.walk filter/map", () -> StackWalker.getInstance().walk(SbrW4Fjp::frames));
        rowx("sw.walk deep", () -> deep(5));
        rowx("sw.walk count>=3", () -> StackWalker.getInstance().walk(s -> s.count()) >= 3);
        rowx("sw.walk first frame", () -> StackWalker.getInstance().walk(s -> s.findFirst())
                .map(f -> f.getClassName() + "." + f.getMethodName()).orElse("none"));
        rowx("sw.walk skip/limit", () -> StackWalker.getInstance()
                .walk(s -> s.skip(1).limit(1).map(StackWalker.StackFrame::getMethodName).collect(Collectors.toList())));
        rowx("sw.frame class", () -> StackWalker.getInstance().walk(s -> s.findFirst()).get().getClass().getName());
        rowx("sw.frame toStackTraceElement", () -> StackWalker.getInstance().walk(s -> s.findFirst()).get()
                .toStackTraceElement().getMethodName());
        rowx("sw.frame toString prefix", () -> {
            String t = StackWalker.getInstance().walk(s -> s.findFirst()).get().toString();
            return t.substring(0, t.indexOf('('));
        });
        rowx("sw.frame bci>=0 native", () -> {
            var f = StackWalker.getInstance().walk(s -> s.findFirst()).get();
            return (f.getByteCodeIndex() >= 0) + " " + f.isNativeMethod() + " " + f.getFileName();
        });
        rowx("sw.frame getDeclaringClass no-retain", () -> StackWalker.getInstance().walk(s -> s.findFirst()).get()
                .getDeclaringClass());
        rowx("sw.frame getDeclaringClass retain", () -> StackWalker
                .getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE).walk(s -> s.findFirst()).get()
                .getDeclaringClass().getName());
        rowx("sw.frame descriptor", () -> StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE)
                .walk(s -> s.findFirst()).get().getDescriptor());
        rowx("sw.getInstance(Set)", () -> StackWalker.getInstance(EnumSet.of(StackWalker.Option.RETAIN_CLASS_REFERENCE,
                StackWalker.Option.SHOW_REFLECT_FRAMES)).walk(s -> s.findFirst()).get().getDeclaringClass().getSimpleName());
        rowx("sw.getInstance(Set,int)", () -> StackWalker.getInstance(Set.of(), 4).walk(s -> s.limit(1).count()));
        rowx("sw.getInstance(Set,0) IAE", () -> StackWalker.getInstance(Set.of(), 0));
        rowx("sw.getCallerClass", () -> callerOf().getName());
        rowx("sw.getCallerClass no-retain", () -> StackWalker.getInstance().getCallerClass());
        rowx("sw.forEach", () -> {
            List<String> names = new ArrayList<>();
            StackWalker.getInstance().forEach(f -> {
                if (f.getClassName().startsWith("SbrW4Fjp") && names.size() < 2) names.add(f.getMethodName());
            });
            return names;
        });
        rowx("sw.walk null fn NPE", () -> StackWalker.getInstance().walk(null));
        rowx("sw.stream escapes", () -> {
            java.util.stream.Stream<StackWalker.StackFrame> leaked = StackWalker.getInstance().walk(s -> s);
            try {
                return leaked.count();
            } catch (IllegalStateException e) {
                return "ISE";
            }
        });

        // ------------------------------------------------ Runtime / Version
        rowx("rt.getRuntime same", () -> Runtime.getRuntime() == Runtime.getRuntime());
        rowx("rt.availableProcessors>0", () -> Runtime.getRuntime().availableProcessors() > 0);
        rowx("rt.version same", () -> Runtime.version() == Runtime.version());
        rowx("rt.version feature==spec", () -> Runtime.version().feature()
                == Integer.parseInt(System.getProperty("java.specification.version")));
        rowx("rt.version toString==java.runtime.version",
                () -> Runtime.version().toString().equals(System.getProperty("java.runtime.version")));
        rowx("rt.version build present", () -> Runtime.version().build().isPresent());
        rowx("rt.version pre/optional", () -> Runtime.version().pre() + " " + Runtime.version().optional().isPresent());
        rowx("rt.version class", () -> Runtime.version().getClass().getName());
        rowx("rt.version compareTo self", () -> Runtime.version().compareTo(Runtime.version()));
        rowx("rt.version equals parse(toString)",
                () -> Runtime.version().equals(Runtime.Version.parse(Runtime.version().toString())));
        rowx("rt.Version.parse", () -> {
            Runtime.Version v = Runtime.Version.parse("21.0.1-ea+12-LTS");
            return v.feature() + " " + v.interim() + " " + v.update() + " " + v.pre() + " " + v.build() + " "
                    + v.optional() + " " + v.version();
        });
        rowx("rt.Version.parse compare", () -> Runtime.Version.parse("17.0.2").compareTo(Runtime.Version.parse("17.0.10")));
        rowx("rt.hook add/remove", () -> {
            Thread t = new Thread(() -> {});
            Runtime.getRuntime().addShutdownHook(t);
            boolean r1 = Runtime.getRuntime().removeShutdownHook(t);
            boolean r2 = Runtime.getRuntime().removeShutdownHook(t);
            return r1 + " " + r2;
        });
        rowx("rt.hook add twice IAE", () -> {
            Thread t = new Thread(() -> {});
            Runtime.getRuntime().addShutdownHook(t);
            try {
                Runtime.getRuntime().addShutdownHook(t);
                return "no-throw";
            } catch (IllegalArgumentException e) {
                return "IAE " + e.getMessage();
            } finally {
                Runtime.getRuntime().removeShutdownHook(t);
            }
        });
        rowx("rt.hook null NPE", () -> {
            Runtime.getRuntime().addShutdownHook(null);
            return "no-throw";
        });
        rowx("rt.hook started IAE", () -> {
            Thread t = new Thread(() -> {});
            t.start();
            t.join();
            try {
                Runtime.getRuntime().addShutdownHook(t);
                return "no-throw";
            } catch (IllegalArgumentException e) {
                return "IAE " + e.getMessage();
            }
        });

        // ------------------------------------------------ Signal
        rowx("sig INT", () -> {
            sun.misc.Signal s = new sun.misc.Signal("INT");
            return s.getName() + " " + s.getNumber() + " " + s;
        });
        rowx("sig TERM equals", () -> new sun.misc.Signal("TERM").equals(new sun.misc.Signal("TERM")));
        rowx("sig SIGINT IAE", () -> new sun.misc.Signal("SIGINT"));
        rowx("sig NOPE IAE", () -> new sun.misc.Signal("NOPE"));
        rowx("sig null NPE", () -> new sun.misc.Signal(null));

        String[] first = new String[ROWS.size()];
        for (int i = 0; i < ROWS.size(); i++) {
            first[i] = safe(ROWS.get(i));
            System.out.println("row " + LABELS.get(i) + " = " + first[i]);
        }
        int drift = 0;
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 200;
        for (int k = 0; k < iters; k++) {
            for (int i = 0; i < ROWS.size(); i++) {
                String v = safe(ROWS.get(i));
                if (!v.equals(first[i])) {
                    if (drift < 10) {
                        System.out.println("DRIFT at iter " + k + " " + LABELS.get(i) + " = " + v);
                    }
                    drift++;
                }
            }
        }
        System.out.println("rows " + ROWS.size() + " drift " + drift);
        Runtime.getRuntime().addShutdownHook(new Thread(() -> System.out.println("hook ran")));
    }
}
