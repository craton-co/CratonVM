// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.PhantomReference;
import java.lang.ref.Reference;
import java.lang.ref.ReferenceQueue;
import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.ConcurrentLinkedQueue;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/**
 * SBR round 5, lane fix3: HotSpot-diffed probe of the untimed ReferenceQueue.remove(): an
 * available reference (also with the interrupt flag set), a waiter woken by enqueue() and
 * by a garbage collection, an interrupt of an empty-queue waiter, several waiters and
 * several references, a waiter parked across collections, and the timed overload.
 * Run in both modes.
 *
 * Run: javac -d out Sbr5Fix3QueueRemove.java
 *      cratonvm --java-home <jdk25> [--compatible] -cp out Sbr5Fix3QueueRemove
 */
public class Sbr5Fix3QueueRemove {
    static void say(String s) { System.out.println(s); }

    static Thread waiter(String name, ReferenceQueue<Object> q, ConcurrentLinkedQueue<String> out,
                         CountDownLatch started) {
        Thread t = new Thread(() -> {
            started.countDown();
            try {
                Reference<?> r = q.remove();
                out.add(name + " got " + (r == null ? "null" : r.getClass().getSimpleName()));
            } catch (InterruptedException e) {
                out.add(name + " interrupted flag=" + Thread.currentThread().isInterrupted());
            }
        }, name);
        t.setDaemon(true);
        return t;
    }

    static void waitState(Thread t) throws Exception {
        for (int i = 0; i < 500 && t.getState() == Thread.State.RUNNABLE; i++) Thread.sleep(2);
        Thread.sleep(50);
    }

    public static void main(String[] args) throws Throwable {
        // 1. Available reference.
        ReferenceQueue<Object> q = new ReferenceQueue<>();
        Object k = new Object();
        WeakReference<Object> w = new WeakReference<>(k, q);
        say("enqueue: " + w.enqueue());
        say("available: " + (q.remove() == w));
        // 2. Available reference with the interrupt flag set: returned, flag kept.
        WeakReference<Object> w2 = new WeakReference<>(k, q);
        w2.enqueue();
        Thread.currentThread().interrupt();
        Reference<?> got = q.remove();
        say("available+interrupt: " + (got == w2) + " flag=" + Thread.currentThread().isInterrupted());
        // 3. Empty queue with the flag set: InterruptedException, flag cleared.
        try {
            q.remove();
            say("empty+interrupt: returned");
        } catch (InterruptedException e) {
            say("empty+interrupt: IE flag=" + Thread.currentThread().isInterrupted() + " msg=" + e.getMessage());
        }
        // 4. Waiter woken by enqueue().
        ConcurrentLinkedQueue<String> out = new ConcurrentLinkedQueue<>();
        CountDownLatch st = new CountDownLatch(1);
        Thread t1 = waiter("w1", q, out, st);
        t1.start();
        st.await();
        waitState(t1);
        say("waiter state: " + t1.getState());
        new PhantomReference<>(k, q).enqueue();
        t1.join(5000);
        say("woken: " + out + " alive=" + t1.isAlive());
        out.clear();
        // 5. Waiter interrupted while parked.
        st = new CountDownLatch(1);
        Thread t2 = waiter("w2", q, out, st);
        t2.start();
        st.await();
        waitState(t2);
        t2.interrupt();
        t2.join(5000);
        say("interrupted: " + out + " alive=" + t2.isAlive());
        out.clear();
        // 6. Waiter woken by a collection.
        ReferenceQueue<Object> gq = new ReferenceQueue<>();
        st = new CountDownLatch(1);
        Thread t3 = waiter("w3", gq, out, st);
        t3.start();
        st.await();
        waitState(t3);
        List<Object> keep = new ArrayList<>();
        WeakReference<Object> gw = new WeakReference<>(new byte[1000], gq);
        for (int i = 0; i < 20 && t3.isAlive(); i++) {
            System.gc();
            t3.join(200);
        }
        say("gc-woken: " + out + " alive=" + t3.isAlive() + " cleared=" + (gw.get() == null));
        out.clear();
        // 7. Several waiters, several references.
        ReferenceQueue<Object> mq = new ReferenceQueue<>();
        int n = 6;
        CountDownLatch many = new CountDownLatch(n);
        List<Thread> ts = new ArrayList<>();
        ConcurrentLinkedQueue<String> outs = new ConcurrentLinkedQueue<>();
        for (int i = 0; i < n; i++) {
            Thread t = waiter("m" + i, mq, outs, many);
            ts.add(t);
            t.start();
        }
        many.await();
        Thread.sleep(100);
        for (int i = 0; i < n; i++) new WeakReference<>(k, mq).enqueue();
        int done = 0;
        for (Thread t : ts) { t.join(5000); if (!t.isAlive()) done++; }
        say("many: finished=" + done + "/" + n + " results=" + outs.size());
        // 8. A waiter parked across several collections still gets a late reference.
        ReferenceQueue<Object> lq = new ReferenceQueue<>();
        st = new CountDownLatch(1);
        Thread t4 = waiter("w4", lq, out, st);
        t4.start();
        st.await();
        for (int i = 0; i < 5; i++) {
            byte[][] junk = new byte[200][];
            for (int j = 0; j < junk.length; j++) junk[j] = new byte[10000];
            System.gc();
        }
        waitState(t4);
        say("parked state: " + t4.getState());
        new WeakReference<>(k, lq).enqueue();
        t4.join(5000);
        say("late: " + out + " alive=" + t4.isAlive());
        out.clear();
        // 9. Timed overload on an empty queue, and a negative timeout.
        say("timed-empty: " + new ReferenceQueue<Object>().remove(30));
        try { new ReferenceQueue<Object>().remove(-1); } catch (IllegalArgumentException e) { say("negative: " + e.getMessage()); }
        // 10. remove() on a subclass queue.
        class Q extends ReferenceQueue<Object> { }
        Q sq = new Q();
        new WeakReference<>(k, sq).enqueue();
        say("subclass: " + (sq.remove() != null));
        say("done " + keep.size());
    }
}
