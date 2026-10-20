// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 74, lane L1: `LinkedBlockingQueue.drainTo` on a
// real JDK
// (docs/internal/fixed-bugs/interpreter-i74-L1-real-jdk-lbq-drain-to-stand-in-reads-the-synthetic-layout-FIXED-20261111.md).
// Both real-JDK arms of `vm_init` register `Bridge` stand-ins for
// `LinkedBlockingQueue.drainTo(Collection, int)` and
// `BlockingQueue.drainTo(..)` that read slot 1 as an `int` size and slot 0 as
// the element array -- the synthetic library's layout. On JDK 25's class slot
// 0 is `capacity` and slot 1 is `count` (an `AtomicInteger`), so from code
// reading the stand-in drains nothing and answers 0. The rows: a bounded and
// an unbounded drain, `drainTo(Collection)` (whose JDK body calls
// `drainTo(c, Integer.MAX_VALUE)`), the queue's state after each, the call
// through the `BlockingQueue` static type, an `ArrayBlockingQueue` control,
// SLF4J's `replayEvents` loop, and `ThreadPoolExecutor.shutdownNow()`.
//
// Run: `cratonvm L1W74BlockingQueueDrainTo` and `cratonvm --compatible
// L1W74BlockingQueueDrainTo`; both must print the block below.
//
// The expected block is DERIVED from the JDK 25 sources, not yet captured
// from the host oracle: refresh it from the host's HotSpot run before reading
// a difference.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// lbq drainTo(c, 2): n=2 sink=[a, b] queue=[c] size=1
// lbq drainTo(c): n=2 sink=[a, b, c, d] queue=[] size=0
// lbq bounded: offers=true,true,false n=2 remaining=2 sink=[x, y]
// BlockingQueue drainTo(c, 1): n=1 sink=[p] queue=[q]
// abq drainTo(c, 5): n=2 sink=[a, b] queue=[]
// replay loop: drained=300 rounds=3
// tpe shutdownNow: 3
// done
// HOTSPOT_EXPECTED_END
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.BlockingQueue;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.ThreadPoolExecutor;
import java.util.concurrent.TimeUnit;

public class L1W74BlockingQueueDrainTo {
    public static void main(String[] args) throws Exception {
        LinkedBlockingQueue<String> q = new LinkedBlockingQueue<>(List.of("a", "b", "c"));
        List<String> sink = new ArrayList<>();
        int n = q.drainTo(sink, 2);
        System.out.println("lbq drainTo(c, 2): n=" + n + " sink=" + sink + " queue=" + q + " size=" + q.size());
        q.offer("d");
        n = q.drainTo(sink);
        System.out.println("lbq drainTo(c): n=" + n + " sink=" + sink + " queue=" + q + " size=" + q.size());

        LinkedBlockingQueue<String> bounded = new LinkedBlockingQueue<>(2);
        boolean o1 = bounded.offer("x");
        boolean o2 = bounded.offer("y");
        boolean o3 = bounded.offer("z");
        List<String> sink2 = new ArrayList<>();
        n = bounded.drainTo(sink2, 10);
        System.out.println("lbq bounded: offers=" + o1 + "," + o2 + "," + o3 + " n=" + n
                + " remaining=" + bounded.remainingCapacity() + " sink=" + sink2);

        BlockingQueue<String> bq = new LinkedBlockingQueue<>(List.of("p", "q"));
        List<String> sink3 = new ArrayList<>();
        n = bq.drainTo(sink3, 1);
        System.out.println("BlockingQueue drainTo(c, 1): n=" + n + " sink=" + sink3 + " queue=" + bq);

        BlockingQueue<String> abq = new ArrayBlockingQueue<>(3);
        abq.add("a");
        abq.add("b");
        List<String> sink4 = new ArrayList<>();
        n = abq.drainTo(sink4, 5);
        System.out.println("abq drainTo(c, 5): n=" + n + " sink=" + sink4 + " queue=" + abq);

        // SLF4J LoggerFactory.replayEvents: drain in batches of 128 until empty.
        LinkedBlockingQueue<Integer> events = new LinkedBlockingQueue<>();
        for (int i = 0; i < 300; i++) {
            events.add(i);
        }
        int drained = 0;
        int rounds = 0;
        while (true) {
            List<Integer> batch = new ArrayList<>(128);
            int got = events.drainTo(batch, 128);
            if (got == 0) {
                break;
            }
            drained += got;
            rounds++;
        }
        System.out.println("replay loop: drained=" + drained + " rounds=" + rounds);

        CountDownLatch started = new CountDownLatch(1);
        CountDownLatch never = new CountDownLatch(1);
        ThreadPoolExecutor tpe = new ThreadPoolExecutor(1, 1, 0L, TimeUnit.MILLISECONDS, new LinkedBlockingQueue<>());
        tpe.execute(() -> {
            started.countDown();
            try {
                never.await();
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
        });
        started.await();
        for (int i = 0; i < 3; i++) {
            tpe.execute(() -> { });
        }
        List<Runnable> pending = tpe.shutdownNow();
        tpe.awaitTermination(5, TimeUnit.SECONDS);
        System.out.println("tpe shutdownNow: " + pending.size());
        System.out.println("done");
    }
}
