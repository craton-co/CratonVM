import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.Queue;
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.BlockingQueue;
import java.util.concurrent.ConcurrentLinkedDeque;
import java.util.concurrent.ConcurrentLinkedQueue;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.LinkedBlockingDeque;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.ThreadPoolExecutor;
import java.util.concurrent.TimeUnit;

/**
 * Regression: the synthetic blocking-queue natives were applied to real JDK
 * queue objects, so `offer` returned true into a four-slot side layout the real
 * bytecode could not see.
 *
 * `size()` read 0, `peek()` was null, and `poll(timeout)`/`take()` NPE'd on a
 * `takeLock` the synthetic `<init>` never assigned. Only `LinkedBlockingDeque`
 * had been exempted; `LinkedBlockingQueue`, `ArrayBlockingQueue` and the
 * `ConcurrentLinked*` pair had not.
 *
 * That is load-bearing rather than cosmetic, because `ThreadPoolExecutor`'s
 * work queue IS a `LinkedBlockingQueue`: `execute()` queued every task past
 * `corePoolSize` into a store no worker could take from, so exactly
 * `corePoolSize` tasks ran, the rest vanished with no exception, and the pool
 * never reached TERMINATED. `newSingleThreadExecutor()` resolved 1 of 6
 * futures; `newFixedThreadPool(4)` resolved 4 of 8.
 *
 * Only a binary built with the `synthetic-jdk` Cargo feature and RUN against
 * the real JDK could reach it — which is exactly what the vm test gate and this
 * suite are usually run with, so it corrupted measurement rather than shipped
 * behaviour.
 */
public class RBlockingQueue {
    static int checks = 0;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    /** offer/size/peek/poll must agree with each other on every queue in the family. */
    static void coherentSingleThreaded() {
        List<Queue<String>> qs = new ArrayList<>();
        qs.add(new LinkedBlockingQueue<>());
        qs.add(new LinkedBlockingQueue<>(10));
        qs.add(new ArrayBlockingQueue<>(10));
        qs.add(new LinkedBlockingDeque<>());
        qs.add(new ConcurrentLinkedQueue<>());
        qs.add(new ConcurrentLinkedDeque<>());
        for (Queue<String> q : qs) {
            String name = q.getClass().getName();
            check(q.offer("a"), name + ": offer(a) returned false");
            check(q.offer("b"), name + ": offer(b) returned false");
            check(q.offer("c"), name + ": offer(c) returned false");
            check(q.size() == 3, name + ": size=" + q.size() + " after 3 offers");
            check(!q.isEmpty(), name + ": isEmpty after 3 offers");
            check("a".equals(q.peek()), name + ": peek=" + q.peek());
            check("a".equals(q.poll()), name + ": poll did not return the head");
            check(q.size() == 2, name + ": size=" + q.size() + " after one poll");
            int iterated = 0;
            for (String ignored : q) {
                iterated++;
            }
            check(iterated == 2, name + ": iterated " + iterated + " of 2");
        }
    }

    /** The `ThreadPoolExecutor.getTask` shape: one thread takes, another offers. */
    static void blockingTakeAcrossThreads() throws Exception {
        List<BlockingQueue<String>> qs = new ArrayList<>();
        qs.add(new LinkedBlockingQueue<>());
        qs.add(new ArrayBlockingQueue<>(10));
        qs.add(new LinkedBlockingDeque<>());
        for (BlockingQueue<String> q : qs) {
            String name = q.getClass().getName();
            List<String> got = Collections.synchronizedList(new ArrayList<>());
            Thread t = new Thread(() -> {
                try {
                    for (int i = 0; i < 3; i++) {
                        got.add(q.take());
                    }
                } catch (Throwable e) {
                    got.add("ERR:" + e.getClass().getName());
                }
            });
            t.setDaemon(true);
            t.start();
            Thread.sleep(150);
            for (int i = 0; i < 3; i++) {
                q.offer("v" + i);
            }
            t.join(30_000);
            check(got.size() == 3, name + ": consumer took " + got + ", expected 3 values");
            check(!got.get(0).startsWith("ERR"), name + ": consumer threw " + got.get(0));
        }
        // The timed poll is a separate real-bytecode path from take().
        BlockingQueue<String> q = new LinkedBlockingQueue<>();
        q.offer("x");
        check("x".equals(q.poll(5, TimeUnit.SECONDS)), "timed poll did not return the offered value");
        check(q.poll(50, TimeUnit.MILLISECONDS) == null, "timed poll on an empty queue returned a value");
    }

    /** A bounded pool must run EVERY submitted task, not just `corePoolSize` of them. */
    static void poolRunsEveryTask() throws Exception {
        ThreadPoolExecutor p = (ThreadPoolExecutor) Executors.newFixedThreadPool(4);
        List<Future<Integer>> fs = new ArrayList<>();
        for (int i = 0; i < 8; i++) {
            final int k = i;
            fs.add(p.submit(() -> k));
        }
        int sum = 0;
        for (int i = 0; i < 8; i++) {
            sum += fs.get(i).get(60, TimeUnit.SECONDS);
        }
        check(sum == 28, "fixed(4) summed " + sum + ", expected 28 — tasks were dropped");
        check(p.getTaskCount() == 8, "fixed(4) counted " + p.getTaskCount() + " tasks, expected 8");
        p.shutdown();
        check(p.awaitTermination(60, TimeUnit.SECONDS), "fixed(4) never reached TERMINATED");

        ExecutorService s = Executors.newSingleThreadExecutor();
        List<Future<Integer>> sf = new ArrayList<>();
        for (int i = 0; i < 6; i++) {
            final int k = i;
            sf.add(s.submit(() -> k));
        }
        int ssum = 0;
        for (int i = 0; i < 6; i++) {
            ssum += sf.get(i).get(60, TimeUnit.SECONDS);
        }
        check(ssum == 15, "single-thread executor summed " + ssum + ", expected 15");
        s.shutdown();
        check(s.awaitTermination(60, TimeUnit.SECONDS), "single-thread executor never terminated");
    }

    public static void main(String[] args) throws Exception {
        coherentSingleThreaded();
        blockingTakeAcrossThreads();
        poolRunsEveryTask();
        System.out.println("CK RBlockingQueue checks=" + checks);
        System.out.println("PASS RBlockingQueue");
    }
}
