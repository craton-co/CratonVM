// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.locks.ReentrantLock;

/**
 * SBR wave 6, lane {@code thread}: timing for the {@code java.lang.Thread} rows
 * that sit on hot paths (accessors, the interrupt-status polls AQS makes on
 * every blocking call, thread start/join, sleep(0)/yield).
 *
 * <pre>
 *   cratonvm --jdk-only -cp . SbrW6ThreadBench [rounds]
 * </pre>
 * Prints ns/op, best and median of the rounds, per row. Rows are sized so a
 * round takes tens of milliseconds on the fast side; the sink keeps the JIT
 * from deleting the loops.
 */
public class SbrW6ThreadBench {
    interface Body {
        long run(int n) throws Exception;
    }

    static long sink;

    static void bench(String name, int n, int rounds, Body b) throws Exception {
        // warm-up: enough to get the row compiled where the VM compiles it.
        sink += b.run(Math.max(n / 4, 1));
        sink += b.run(Math.max(n / 4, 1));
        double[] ns = new double[rounds];
        for (int r = 0; r < rounds; r++) {
            long t0 = System.nanoTime();
            sink += b.run(n);
            ns[r] = (System.nanoTime() - t0) / (double) n;
        }
        java.util.Arrays.sort(ns);
        System.out.printf("%-26s best %10.1f  median %10.1f ns/op%n", name, ns[0], ns[rounds / 2]);
    }

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        Thread cur = Thread.currentThread();
        Runnable nop = () -> {};

        bench("getName", 2_000_000, rounds, n -> {
            long s = 0;
            for (int i = 0; i < n; i++) s += cur.getName().length();
            return s;
        });
        bench("getPriority", 2_000_000, rounds, n -> {
            long s = 0;
            for (int i = 0; i < n; i++) s += cur.getPriority();
            return s;
        });
        bench("isDaemon", 2_000_000, rounds, n -> {
            long s = 0;
            for (int i = 0; i < n; i++) s += cur.isDaemon() ? 1 : 0;
            return s;
        });
        bench("getThreadGroup", 2_000_000, rounds, n -> {
            long s = 0;
            for (int i = 0; i < n; i++) s += cur.getThreadGroup() != null ? 1 : 0;
            return s;
        });
        bench("getContextClassLoader", 2_000_000, rounds, n -> {
            long s = 0;
            for (int i = 0; i < n; i++) s += cur.getContextClassLoader() != null ? 1 : 0;
            return s;
        });
        bench("getState(current)", 500_000, rounds, n -> {
            long s = 0;
            for (int i = 0; i < n; i++) s += cur.getState().ordinal();
            return s;
        });
        bench("isAlive(current)", 2_000_000, rounds, n -> {
            long s = 0;
            for (int i = 0; i < n; i++) s += cur.isAlive() ? 1 : 0;
            return s;
        });
        bench("isInterrupted", 2_000_000, rounds, n -> {
            long s = 0;
            for (int i = 0; i < n; i++) s += cur.isInterrupted() ? 1 : 0;
            return s;
        });
        bench("Thread.interrupted()", 2_000_000, rounds, n -> {
            long s = 0;
            for (int i = 0; i < n; i++) s += Thread.interrupted() ? 1 : 0;
            return s;
        });
        bench("interrupt+interrupted", 200_000, rounds, n -> {
            long s = 0;
            for (int i = 0; i < n; i++) {
                cur.interrupt();
                s += Thread.interrupted() ? 1 : 0;
            }
            return s;
        });
        bench("setName(same)", 500_000, rounds, n -> {
            String nm = cur.getName();
            for (int i = 0; i < n; i++) cur.setName(nm);
            return n;
        });
        bench("setDaemon(new thread)", 100_000, rounds, n -> {
            long s = 0;
            Thread t = new Thread(nop);
            for (int i = 0; i < n; i++) {
                t.setDaemon((i & 1) == 0);
                s += t.isDaemon() ? 1 : 0;
            }
            return s;
        });
        bench("new Thread(r)", 20_000, rounds, n -> {
            long s = 0;
            for (int i = 0; i < n; i++) s += new Thread(nop).getPriority();
            return s;
        });
        bench("new Thread(g,r,n,0)", 20_000, rounds, n -> {
            ThreadGroup g = cur.getThreadGroup();
            long s = 0;
            for (int i = 0; i < n; i++) s += new Thread(g, nop, "x", 0L).getPriority();
            return s;
        });
        bench("start+join", 2_000, rounds, n -> {
            long s = 0;
            for (int i = 0; i < n; i++) {
                Thread t = new Thread(nop);
                t.start();
                t.join();
                s++;
            }
            return s;
        });
        bench("Thread.yield()", 200_000, rounds, n -> {
            for (int i = 0; i < n; i++) Thread.yield();
            return n;
        });
        bench("Thread.sleep(0)", 100_000, rounds, n -> {
            for (int i = 0; i < n; i++) Thread.sleep(0);
            return n;
        });
        bench("Thread.sleep(1)", 300, rounds, n -> {
            for (int i = 0; i < n; i++) Thread.sleep(1);
            return n;
        });
        bench("lockInterruptibly", 500_000, rounds, n -> {
            ReentrantLock l = new ReentrantLock();
            for (int i = 0; i < n; i++) {
                l.lockInterruptibly();
                l.unlock();
            }
            return n;
        });
        bench("ABQ pingpong", 20_000, rounds, n -> {
            ArrayBlockingQueue<Integer> a = new ArrayBlockingQueue<>(1);
            ArrayBlockingQueue<Integer> b = new ArrayBlockingQueue<>(1);
            Thread t = new Thread(() -> {
                try {
                    for (int i = 0; i < n; i++) b.put(a.take());
                } catch (InterruptedException e) {
                }
            });
            t.start();
            long s = 0;
            for (int i = 0; i < n; i++) {
                a.put(i);
                s += b.take();
            }
            t.join();
            return s;
        });
        bench("latch await x2 threads", 5_000, rounds, n -> {
            long s = 0;
            for (int i = 0; i < n; i++) {
                CountDownLatch l = new CountDownLatch(1);
                Thread t = new Thread(l::countDown);
                t.start();
                l.await();
                s++;
            }
            return s;
        });
        System.out.println("sink " + (sink != 0));
    }
}
