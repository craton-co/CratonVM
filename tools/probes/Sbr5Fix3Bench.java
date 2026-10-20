// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;
import java.lang.ref.Reference;
import java.lang.ref.ReferenceQueue;
import java.lang.ref.WeakReference;

/**
 * SBR round 5, lane fix3: cost of the two retired rows. Prints one number per row, the
 * wall nanoseconds per operation (the caller measures CPU time with /usr/bin/time).
 * Args: indirect | queue | queuewait.
 *  indirect  : IndirectVarHandle.withInvokeExactBehavior() x N.
 *  queue     : remove() on a non-empty queue x N (the enqueue side is the same in both arms).
 *  queuewait : a ping-pong: main enqueues, a waiter thread remove()s it, N round trips.
 */
public class Sbr5Fix3Bench {
    long l = 40;
    static long id(long x) { return x; }

    public static void main(String[] args) throws Throwable {
        String what = args.length > 0 ? args[0] : "indirect";
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 200000;
        Object k = new Object();
        switch (what) {
            case "indirect": {
                MethodHandles.Lookup lk = MethodHandles.lookup();
                VarHandle vl = lk.findVarHandle(Sbr5Fix3Bench.class, "l", long.class);
                MethodHandle id = lk.findStatic(Sbr5Fix3Bench.class, "id", MethodType.methodType(long.class, long.class));
                VarHandle plain = MethodHandles.filterValue(vl, id, id);
                for (int r = 0; r < 3; r++) {
                    long t0 = System.nanoTime();
                    int c = 0;
                    for (int i = 0; i < n; i++) if (plain.withInvokeExactBehavior().hasInvokeExactBehavior()) c++;
                    long t1 = System.nanoTime();
                    System.out.println("indirect round " + r + ": " + (t1 - t0) / n + " ns/op " + c);
                }
                break;
            }
            case "queue": {
                ReferenceQueue<Object> q = new ReferenceQueue<>();
                WeakReference<Object>[] refs = new WeakReference[1024];
                for (int i = 0; i < refs.length; i++) refs[i] = new WeakReference<>(k, q);
                for (int r = 0; r < 3; r++) {
                    long t0 = System.nanoTime();
                    long c = 0;
                    for (int i = 0; i < n; i++) {
                        WeakReference<Object> w = new WeakReference<>(k, q);
                        w.enqueue();
                        if (q.remove() == w) c++;
                    }
                    long t1 = System.nanoTime();
                    System.out.println("queue round " + r + ": " + (t1 - t0) / n + " ns/op " + c);
                }
                break;
            }
            default: {
                ReferenceQueue<Object> q = new ReferenceQueue<>();
                ReferenceQueue<Object> back = new ReferenceQueue<>();
                int m = n / 20;
                Thread t = new Thread(() -> {
                    try {
                        for (int i = 0; i < m * 3; i++) {
                            Reference<?> r = q.remove();
                            new WeakReference<>(k, back).enqueue();
                        }
                    } catch (InterruptedException e) { }
                });
                t.start();
                for (int r = 0; r < 3; r++) {
                    long t0 = System.nanoTime();
                    for (int i = 0; i < m; i++) {
                        new WeakReference<>(k, q).enqueue();
                        back.remove();
                    }
                    long t1 = System.nanoTime();
                    System.out.println("queuewait round " + r + ": " + (t1 - t0) / m + " ns/roundtrip");
                }
                t.join();
            }
        }
    }
}
