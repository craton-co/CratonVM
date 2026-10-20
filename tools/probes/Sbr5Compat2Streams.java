import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.*;
import java.util.stream.*;

/**
 * SBR round 5, lane compat2: parallel streams run on the ForkJoin common pool (workers plus the
 * caller), as HotSpot does. Every line must be byte-identical under HotSpot, --jdk-only and
 * --compatible. Thread-dependent facts are printed as booleans (workers seen, caller seen).
 */
public class Sbr5Compat2Streams {
    static String norm(Object v) {
        return String.valueOf(v).replaceAll("@[0-9a-f]+", "@X").replaceAll("-worker-[0-9]+", "-worker-N");
    }

    static void row(String name, Object v) {
        System.out.println(name + " |" + norm(v) + "|");
    }

    interface T {
        Object run() throws Throwable;
    }

    static void t(String name, T r) {
        try {
            row(name, r.run());
        } catch (Throwable e) {
            row(name, "THREW " + e.getClass().getName() + ": " + norm(e.getMessage()));
        }
    }

    static boolean worker() {
        return Thread.currentThread() instanceof ForkJoinWorkerThread;
    }

    public static void main(String[] args) throws Throwable {
        t("isParallel", () -> IntStream.range(0, 10).parallel().isParallel());
        t("sequential", () -> IntStream.range(0, 10).parallel().sequential().isParallel());
        t("list.parallelStream.isParallel", () -> List.of(1, 2, 3).parallelStream().isParallel());
        t("int.sum", () -> IntStream.range(0, 100000).parallel().sum());
        t("long.sum", () -> LongStream.rangeClosed(1, 100000).parallel().sum());
        t("obj.reduce", () -> IntStream.range(0, 5000).boxed().parallel().reduce(0, Integer::sum));
        t("collect.list", () -> IntStream.range(0, 3000).boxed().parallel().collect(Collectors.toList()).size());
        t("collect.ordered", () -> IntStream.range(0, 3000).boxed().parallel().map(i -> i * 2)
                .collect(Collectors.toList()).subList(0, 6));
        t("sorted", () -> IntStream.range(0, 3000).boxed().sorted(Comparator.reverseOrder()).parallel()
                .limit(5).collect(Collectors.toList()));
        t("forEachOrdered", () -> {
            List<Integer> out = Collections.synchronizedList(new ArrayList<>());
            IntStream.range(0, 2000).parallel().forEachOrdered(out::add);
            return out.subList(0, 8) + " " + out.size();
        });
        t("groupingByConcurrent", () -> IntStream.range(0, 4000).boxed().parallel()
                .collect(Collectors.groupingByConcurrent(i -> i % 3, Collectors.counting())));
        t("forEach.worker.seen", () -> {
            Set<Boolean> seen = ConcurrentHashMap.newKeySet();
            IntStream.range(0, 4000).parallel().forEach(i -> { seen.add(worker()); if (i % 50 == 0) { try { Thread.sleep(1); } catch (InterruptedException e) { } } });
            return seen.contains(true);
        });
        t("forEach.threads>1", () -> {
            Set<Thread> seen = ConcurrentHashMap.newKeySet();
            IntStream.range(0, 4000).parallel().forEach(i -> {
                seen.add(Thread.currentThread());
                if (i % 100 == 0) {
                    try { Thread.sleep(1); } catch (InterruptedException e) { }
                }
            });
            return seen.size() > 1 || Runtime.getRuntime().availableProcessors() == 1;
        });
        t("list.parallelStream.worker", () -> {
            Set<Boolean> seen = ConcurrentHashMap.newKeySet();
            IntStream.range(0, 4000).boxed().collect(Collectors.toList()).parallelStream()
                    .forEach(i -> { seen.add(worker()); if (i % 50 == 0) { try { Thread.sleep(1); } catch (InterruptedException e) { } } });
            return seen.contains(true);
        });
        t("map.worker.names", () -> {
            Set<String> names = ConcurrentHashMap.newKeySet();
            IntStream.range(0, 4000).parallel().map(i -> {
                Thread th = Thread.currentThread();
                if (th instanceof ForkJoinWorkerThread) {
                    names.add(th.getName().replaceAll("[0-9]+$", "N") + " " + th.isDaemon());
                }
                return i;
            }).sum();
            return names;
        });
        t("anyMatch", () -> IntStream.range(0, 100000).parallel().anyMatch(i -> i == 99999));
        t("findAny.present", () -> IntStream.range(0, 100000).parallel().filter(i -> i > 5).findAny().isPresent());
        t("findFirst", () -> IntStream.range(0, 100000).parallel().filter(i -> i > 5).findFirst().getAsInt());
        t("distinct", () -> IntStream.range(0, 5000).map(i -> i % 7).boxed().parallel().distinct().count());
        t("spliterator.trySplit", () -> IntStream.range(0, 100).parallel().spliterator().trySplit() != null);
        t("flatMap", () -> IntStream.range(0, 200).boxed().parallel().flatMap(i -> Stream.of(i, i)).count());
        t("custom.pool.worker", () -> {
            ForkJoinPool p = new ForkJoinPool(3);
            try {
                Set<String> names = ConcurrentHashMap.newKeySet();
                p.submit(() -> IntStream.range(0, 4000).parallel().forEach(i -> {
                    Thread th = Thread.currentThread();
                    if (th instanceof ForkJoinWorkerThread w) {
                        names.add(w.getPool() == p ? "own" : "other");
                    }
                })).get();
                return names;
            } finally {
                p.shutdown();
            }
        });
        t("exception.propagates", () -> {
            // Which thread ran element 3000 is not fixed: the exception is the original or a worker-wrapped copy.
            try {
                IntStream.range(0, 4000).parallel().forEach(i -> {
                    if (i == 3000) throw new IllegalStateException("boom" + i);
                });
                return "no throw";
            } catch (IllegalStateException e) {
                return e.getClass().getName() + " " + e.toString().contains("boom3000");
            }
        });
        t("parallel.then.terminal.twice", () -> {
            IntStream s = IntStream.range(0, 10).parallel();
            s.sum();
            return s.sum();
        });
        t("arrays.parallelSort", () -> {
            int[] a = new int[5000];
            for (int i = 0; i < a.length; i++) a[i] = (i * 7919) % 5000;
            Arrays.parallelSort(a);
            return a[0] + " " + a[4999];
        });
        t("arrays.parallelPrefix", () -> {
            int[] a = new int[2000];
            Arrays.fill(a, 1);
            Arrays.parallelPrefix(a, Integer::sum);
            return a[1999];
        });
        t("concurrentMap.forEach", () -> {
            ConcurrentHashMap<Integer, Integer> m = new ConcurrentHashMap<>();
            for (int i = 0; i < 1000; i++) m.put(i, i);
            AtomicLong sum = new AtomicLong();
            m.forEach(1, (k, v) -> sum.addAndGet(v));
            return sum.get();
        });
        t("commonPool.activity", () -> {
            IntStream.range(0, 4000).parallel().forEach(i -> { });
            return ForkJoinPool.commonPool().getPoolSize() > 0 || Runtime.getRuntime().availableProcessors() == 1;
        });
    }
}
