// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 80, lane L1 -- behavioural probe for the
// `JdkCompletableFuture` HotSpot-alignment row
// (`CRATONVM_COMPAT_JDK_COMPLETABLE_FUTURE`, default OFF): with it, a real-JDK
// `--compatible` VM runs the JDK's own `CompletableFuture` bodies, as
// `--jdk-only` does (where the eleven rows are retired shadows). Without it
// the stand-ins answer `allOf()` / `anyOf()` of no element from the VM's
// two-slot model, `anyOf` of real futures delegates to a JDK `orTree` helper,
// and `thenCombine` throws its own `NullPointerException`. The `completeValue`
// stand-in (reached by `completeAsync`'s `AsyncSupply.run`) was a plain read
// then store that could overwrite a racing `complete(v)` that answered `true`;
// wave 80 made it a CAS in both arms, so `complete-race` must be SAME either way.
//
// Run; jdk-only and the row-on arm must both print the block below:
//   cratonvm L1W80CompletableFutureBodies
//   cratonvm --nojit L1W80CompletableFutureBodies
//   CRATONVM_COMPAT_JDK_COMPLETABLE_FUTURE=1 cratonvm --compatible L1W80CompletableFutureBodies
//   cratonvm --compatible L1W80CompletableFutureBodies     (row off: the any-of and
//                                                          then-combine rows are expected to differ)
//
// The expected block is DERIVED from the JDK 25 sources (CompletableFuture,
// helpful NullPointerException texts), not captured from the host oracle:
// refresh it from the host's HotSpot run before reading a difference.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// all-of-empty: done=true join=null string=[Completed normally]
// all-of-pending: done=false afterOne=false afterBoth=true join=null
// all-of-exceptional: java.util.concurrent.CompletionException cause=java.lang.IllegalStateException: boom
// any-of-empty: done=false string=[Incomplete]
// any-of-one: done=true join=solo
// any-of-completed: done=true join=a
// any-of-later: before=false join=q
// any-of-exceptional: java.util.concurrent.CompletionException cause=java.lang.IllegalArgumentException: bad
// then-combine: join=5
// then-combine-null-other: java.lang.NullPointerException helpful=true top=java.util.concurrent.CompletableFuture.biApplyStage
// then-combine-null-fn: java.lang.NullPointerException message=null top=java.util.concurrent.CompletableFuture.biApplyStage
// complete-null: first=true second=false done=true join=null string=[Completed normally]
// complete-exceptionally-null: java.lang.NullPointerException message=null
// complete-exceptionally: first=true second=false exceptionally=true string=[Completed exceptionally: java.lang.IllegalStateException: x]
// cancel: cancelled=true exceptionally=true done=true
// waiter: got=w
// then-apply-async: value=7 commonPool=true
// complete-race: futures=2000 mismatches=0
// done
// HOTSPOT_EXPECTED_END
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionException;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.Executor;
import java.util.concurrent.TimeUnit;

public class L1W80CompletableFutureBodies {

    static String state(CompletableFuture<?> f) {
        String s = f.toString();
        return s.substring(s.indexOf('['));
    }

    static String top(Throwable t) {
        StackTraceElement[] st = t.getStackTrace();
        return st.length == 0 ? "none" : st[0].getClassName() + "." + st[0].getMethodName();
    }

    static void allOf() {
        CompletableFuture<Void> empty = CompletableFuture.allOf();
        System.out.println("all-of-empty: done=" + empty.isDone() + " join=" + empty.join() + " string="
                + state(empty));

        CompletableFuture<String> a = new CompletableFuture<>();
        CompletableFuture<String> b = new CompletableFuture<>();
        CompletableFuture<Void> both = CompletableFuture.allOf(a, b);
        boolean before = both.isDone();
        a.complete("a");
        boolean afterOne = both.isDone();
        b.complete("b");
        System.out.println("all-of-pending: done=" + before + " afterOne=" + afterOne + " afterBoth="
                + both.isDone() + " join=" + both.join());

        CompletableFuture<String> bad = new CompletableFuture<>();
        bad.completeExceptionally(new IllegalStateException("boom"));
        try {
            CompletableFuture.allOf(CompletableFuture.completedFuture("x"), bad).join();
            System.out.println("all-of-exceptional: no exception");
        } catch (CompletionException e) {
            System.out.println("all-of-exceptional: " + e.getClass().getName() + " cause=" + e.getCause());
        }
    }

    static void anyOf() {
        CompletableFuture<Object> empty = CompletableFuture.anyOf();
        System.out.println("any-of-empty: done=" + empty.isDone() + " string=" + state(empty));

        CompletableFuture<Object> one = CompletableFuture.anyOf(CompletableFuture.completedFuture("solo"));
        System.out.println("any-of-one: done=" + one.isDone() + " join=" + one.join());

        CompletableFuture<Object> completed =
                CompletableFuture.anyOf(new CompletableFuture<String>(), CompletableFuture.completedFuture("a"));
        System.out.println("any-of-completed: done=" + completed.isDone() + " join=" + completed.join());

        CompletableFuture<String> p = new CompletableFuture<>();
        CompletableFuture<String> q = new CompletableFuture<>();
        CompletableFuture<Object> later = CompletableFuture.anyOf(p, q);
        boolean before = later.isDone();
        q.complete("q");
        System.out.println("any-of-later: before=" + before + " join=" + later.join());

        CompletableFuture<String> r = new CompletableFuture<>();
        CompletableFuture<String> s = new CompletableFuture<>();
        CompletableFuture<Object> failed = CompletableFuture.anyOf(r, s);
        s.completeExceptionally(new IllegalArgumentException("bad"));
        try {
            failed.join();
            System.out.println("any-of-exceptional: no exception");
        } catch (CompletionException e) {
            System.out.println("any-of-exceptional: " + e.getClass().getName() + " cause=" + e.getCause());
        }
    }

    static void thenCombine() {
        CompletableFuture<Integer> two = CompletableFuture.completedFuture(2);
        CompletableFuture<Integer> three = CompletableFuture.completedFuture(3);
        System.out.println("then-combine: join=" + two.thenCombine(three, Integer::sum).join());
        try {
            two.thenCombine(null, (x, y) -> x);
            System.out.println("then-combine-null-other: no exception");
        } catch (NullPointerException e) {
            String m = e.getMessage();
            boolean helpful = m != null
                    && m.startsWith("Cannot invoke \"java.util.concurrent.CompletionStage.toCompletableFuture()\"");
            System.out.println("then-combine-null-other: " + e.getClass().getName() + " helpful=" + helpful
                    + " top=" + top(e));
        }
        try {
            two.thenCombine(three, null);
            System.out.println("then-combine-null-fn: no exception");
        } catch (NullPointerException e) {
            System.out.println("then-combine-null-fn: " + e.getClass().getName() + " message=" + e.getMessage()
                    + " top=" + top(e));
        }
    }

    static void completes() {
        CompletableFuture<String> f = new CompletableFuture<>();
        boolean first = f.complete(null);
        boolean second = f.complete("late");
        System.out.println("complete-null: first=" + first + " second=" + second + " done=" + f.isDone()
                + " join=" + f.join() + " string=" + state(f));

        try {
            new CompletableFuture<String>().completeExceptionally(null);
            System.out.println("complete-exceptionally-null: no exception");
        } catch (NullPointerException e) {
            System.out.println("complete-exceptionally-null: " + e.getClass().getName() + " message="
                    + e.getMessage());
        }

        CompletableFuture<String> g = new CompletableFuture<>();
        boolean ex1 = g.completeExceptionally(new IllegalStateException("x"));
        boolean ex2 = g.completeExceptionally(new IllegalStateException("y"));
        System.out.println("complete-exceptionally: first=" + ex1 + " second=" + ex2 + " exceptionally="
                + g.isCompletedExceptionally() + " string=" + state(g));

        CompletableFuture<String> c = new CompletableFuture<>();
        boolean cancelled = c.cancel(true);
        System.out.println("cancel: cancelled=" + cancelled + " exceptionally=" + c.isCompletedExceptionally()
                + " done=" + c.isDone());
    }

    static void waiter() throws Exception {
        CompletableFuture<String> f = new CompletableFuture<>();
        String[] got = new String[1];
        Thread t = new Thread(() -> got[0] = f.join(), "cf-waiter");
        t.start();
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(20);
        while (t.getState() != Thread.State.WAITING) {
            if (System.nanoTime() > deadline) {
                throw new IllegalStateException("waiter never parked");
            }
            Thread.sleep(1);
        }
        f.complete("w");
        t.join();
        System.out.println("waiter: got=" + got[0]);
    }

    static void thenApplyAsync() {
        String[] where = new String[1];
        int v = CompletableFuture.completedFuture(6).thenApplyAsync(x -> {
            where[0] = Thread.currentThread().getName();
            return x + 1;
        }).join();
        System.out.println("then-apply-async: value=" + v + " commonPool="
                + where[0].startsWith("ForkJoinPool.commonPool-worker-"));
    }

    /**
     * `completeAsync`'s `AsyncSupply.run` completes through `completeValue`;
     * a racing `complete(v)` that answers `true` must be the value `join`
     * sees, and one that answers `false` must not be.
     */
    static void completeRace() throws Exception {
        final int n = 2000;
        int mismatches = 0;
        for (int i = 0; i < n; i++) {
            CompletableFuture<String> f = new CompletableFuture<>();
            CountDownLatch start = new CountDownLatch(1);
            Executor racer = r -> {
                Thread t = new Thread(() -> {
                    try {
                        start.await();
                    } catch (InterruptedException e) {
                        return;
                    }
                    r.run();
                }, "cf-racer");
                t.start();
            };
            f.completeAsync(() -> "async", racer);
            start.countDown();
            boolean won = f.complete("main");
            String v = f.join();
            if (won != "main".equals(v)) {
                mismatches++;
            }
        }
        System.out.println("complete-race: futures=" + n + " mismatches=" + mismatches);
    }

    interface Section {
        void run() throws Exception;
    }

    /** A section that throws (a failing stand-in) is reported and the rest still run. */
    static void section(String name, Section s) {
        try {
            s.run();
        } catch (Throwable t) {
            System.out.println(name + " failed: " + t);
        }
    }

    public static void main(String[] args) throws Exception {
        section("all-of", L1W80CompletableFutureBodies::allOf);
        section("any-of", L1W80CompletableFutureBodies::anyOf);
        section("then-combine", L1W80CompletableFutureBodies::thenCombine);
        section("completes", L1W80CompletableFutureBodies::completes);
        section("waiter", L1W80CompletableFutureBodies::waiter);
        section("then-apply-async", L1W80CompletableFutureBodies::thenApplyAsync);
        section("complete-race", L1W80CompletableFutureBodies::completeRace);
        System.out.println("done");
    }
}
