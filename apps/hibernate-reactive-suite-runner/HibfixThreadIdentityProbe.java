// Does Thread.currentThread() keep its identity under JIT?
//
// Vert.x decides whether to run a continuation INLINE or to schedule it with
// EventLoopExecutor.inThread() -> netty's inEventLoop(), which is exactly
// `Thread.currentThread() == this.thread`. A single wrong TRUE there runs a
// session's continuation on a foreign event loop, which is the
// MultithreadedInsertionWithLazyConnectionTest signature.
//
// So this probe reproduces that shape and nothing else: a field holding the
// owning thread, compared by reference against Thread.currentThread(), hot
// enough to be compiled, on many threads at once.
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicLong;

public class HibfixThreadIdentityProbe {

    /** The netty shape: an executor that remembers its thread. */
    static final class Executor {
        volatile Thread thread;
        boolean inEventLoop() { return Thread.currentThread() == thread; }
    }

    static final AtomicLong CHECKS = new AtomicLong();
    static final AtomicLong WRONG_FALSE = new AtomicLong();   // own loop said "not mine"
    static final AtomicLong WRONG_TRUE = new AtomicLong();    // foreign loop said "mine"
    static final AtomicLong NAME_MISMATCH = new AtomicLong();
    static final AtomicLong IDENTITY_MISMATCH = new AtomicLong();

    public static void main(String[] args) throws Exception {
        int threads = Integer.getInteger("probe.threads", 24);
        int iters = Integer.getInteger("probe.iters", 2_000_000);
        Executor[] execs = new Executor[threads];
        for (int i = 0; i < threads; i++) execs[i] = new Executor();
        CountDownLatch ready = new CountDownLatch(threads);
        CountDownLatch go = new CountDownLatch(1);
        CountDownLatch done = new CountDownLatch(threads);

        for (int i = 0; i < threads; i++) {
            final int idx = i;
            Thread t = new Thread(() -> {
                Thread self = Thread.currentThread();
                execs[idx].thread = self;
                String myName = self.getName();
                ready.countDown();
                try { go.await(); } catch (InterruptedException e) { return; }
                for (int n = 0; n < iters; n++) {
                    // 1. my own executor must say yes, every time
                    if (!execs[idx].inEventLoop()) WRONG_FALSE.incrementAndGet();
                    // 2. a neighbour's executor must say no, every time
                    Executor other = execs[(idx + 1 + (n & 7)) % execs.length];
                    if (other != execs[idx] && other.inEventLoop()) WRONG_TRUE.incrementAndGet();
                    // 3. currentThread() must keep its identity and its name
                    Thread now = Thread.currentThread();
                    if (now != self) IDENTITY_MISMATCH.incrementAndGet();
                    if (!now.getName().equals(myName)) NAME_MISMATCH.incrementAndGet();
                    CHECKS.incrementAndGet();
                }
                done.countDown();
            }, "probe-loop-" + i);
            t.start();
        }
        ready.await();
        long t0 = System.nanoTime();
        go.countDown();
        done.await();
        long ms = (System.nanoTime() - t0) / 1_000_000;

        System.out.println("@@PROBE threads=" + threads + " iters=" + iters
                + " checks=" + CHECKS.get()
                + " wrong_false=" + WRONG_FALSE.get()
                + " wrong_true=" + WRONG_TRUE.get()
                + " identity_mismatch=" + IDENTITY_MISMATCH.get()
                + " name_mismatch=" + NAME_MISMATCH.get()
                + " ms=" + ms);
        boolean clean = WRONG_FALSE.get() == 0 && WRONG_TRUE.get() == 0
                && IDENTITY_MISMATCH.get() == 0 && NAME_MISMATCH.get() == 0;
        System.out.println(clean ? "@@PROBE CLEAN" : "@@PROBE DEFECT");
    }
}
