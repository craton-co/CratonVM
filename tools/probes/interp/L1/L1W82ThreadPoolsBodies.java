// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 82, lane L1 -- behavioural probe for the
// `JdkThreadPools` HotSpot-alignment row (`CRATONVM_COMPAT_JDK_THREAD_POOLS`,
// default OFF): with it, a real-JDK `--compatible` VM registers no native on
// `ThreadPoolExecutor` / `ScheduledThreadPoolExecutor`, as `--jdk-only` (where
// the fifteen pool rows are retired shadows and `execute` a refused stub).
// Without it the fifteen routers and the `execute` stub are registered; each
// hands a real pool to `ThreadPoolExecutor`'s own body
// (`invoke_special_bytecode_only`), and the interpreter's doors prefer that body
// anyway (the class is on the real-protected-stub list). So, by code reading,
// every row below is the JDK's answer in BOTH arms: a row that differs with the
// row off names a door that ran the router instead of yielding (the stack of a
// thread waiting in `awaitTermination`, the frames of a rejection, a subclass's
// `super` call, a reflective or method-handle call).
//
// Run; jdk-only, the row-on arm and (expected) the row-off arm print the block:
//   cratonvm L1W82ThreadPoolsBodies
//   cratonvm --nojit L1W82ThreadPoolsBodies
//   CRATONVM_COMPAT_JDK_THREAD_POOLS=1 cratonvm --compatible L1W82ThreadPoolsBodies
//   CRATONVM_COMPAT_JDK_THREAD_POOLS=1 cratonvm --compatible --nojit L1W82ThreadPoolsBodies
//   cratonvm --compatible L1W82ThreadPoolsBodies          (row off)
//
// The expected block is DERIVED from the JDK 25 sources (ThreadPoolExecutor,
// AbstractExecutorService, Executors, ScheduledThreadPoolExecutor,
// AbstractQueuedSynchronizer$ConditionObject), not captured from the host
// oracle: refresh it from the host's HotSpot run before reading a difference.
// Every section that throws prints `<section> failed: ...` and the rest run.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// worker-name: pattern=true daemon=false
// counts-busy: pool=2 active=2 queued=1 tasks=3 completed=0 state=[Running, pool size = 2, active threads = 2, queued tasks = 1, completed tasks = 0]
// counts-done: terminated=true pool=0 largest=2 tasks=3 completed=3 shutdown=true terminating=false state=[Terminated, pool size = 0, active threads = 0, queued tasks = 0, completed tasks = 3]
// shutdown-now: drained=2 kinds=L1W82ThreadPoolsBodies$Named,L1W82ThreadPoolsBodies$Named interrupted=true shutdown=true terminated=true
// rejected: java.util.concurrent.RejectedExecutionException frames=ThreadPoolExecutor$AbortPolicy.rejectedExecution>ThreadPoolExecutor.reject>ThreadPoolExecutor.execute>AbstractExecutorService.submit>L1W82ThreadPoolsBodies.rejected
// rejected-null: java.lang.NullPointerException top=ThreadPoolExecutor.execute
// submit-shapes: callable=c runnable-result=r runnable=null future=java.util.concurrent.FutureTask all=[a, b] any=ok
// subclass: hooks=before,run,after,terminated isShutdownCalls=2 pool=101 core=1
// reflect: core=3 max=4 shutdown=false terminated=false iface-shutdown=false
// await-waiter: state=TIMED_WAITING blocker=java.util.concurrent.locks.AbstractQueuedSynchronizer$ConditionObject frames=Unsafe.park>LockSupport.parkNanos>AbstractQueuedSynchronizer$ConditionObject.awaitNanos>ThreadPoolExecutor.awaitTermination>L1W82ThreadPoolsBodies.awaitWaiter terminated=true
// scheduled: core0=0 handler=true value=s terminated=true
// single: class=java.util.concurrent.Executors$AutoShutdownDelegatedExecutorService isTpe=false value=1
// done
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.Callable;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.RejectedExecutionHandler;
import java.util.concurrent.ScheduledFuture;
import java.util.concurrent.ScheduledThreadPoolExecutor;
import java.util.concurrent.ThreadPoolExecutor;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.locks.LockSupport;

public class L1W82ThreadPoolsBodies {

    static ThreadPoolExecutor pool(int core, int max) {
        return new ThreadPoolExecutor(core, max, 0L, TimeUnit.MILLISECONDS, new LinkedBlockingQueue<Runnable>());
    }

    /** The pool's `toString()` from its `[`: the identity hash masked. */
    static String state(ThreadPoolExecutor p) {
        String s = p.toString();
        return s.substring(s.indexOf('['));
    }

    static String simple(StackTraceElement e) {
        String c = e.getClassName();
        return c.substring(c.lastIndexOf('.') + 1) + "." + e.getMethodName();
    }

    /** Frames from the top down to (and including) the first frame of this class. */
    static String frames(StackTraceElement[] st) {
        StringBuilder sb = new StringBuilder();
        for (StackTraceElement e : st) {
            if (sb.length() > 0) {
                sb.append('>');
            }
            sb.append(simple(e));
            if (e.getClassName().equals(L1W82ThreadPoolsBodies.class.getName())) {
                break;
            }
        }
        return sb.toString();
    }

    static final class Named implements Runnable {
        final CountDownLatch gate;

        Named(CountDownLatch gate) {
            this.gate = gate;
        }

        @Override
        public void run() {
            try {
                gate.await();
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
        }
    }

    static void workerName() throws Exception {
        ThreadPoolExecutor p = pool(1, 1);
        String[] name = new String[1];
        boolean[] daemon = new boolean[1];
        p.submit(() -> {
            name[0] = Thread.currentThread().getName();
            daemon[0] = Thread.currentThread().isDaemon();
        }).get();
        p.shutdown();
        p.awaitTermination(10, TimeUnit.SECONDS);
        System.out.println("worker-name: pattern=" + name[0].matches("pool-[0-9]+-thread-1") + " daemon=" + daemon[0]);
    }

    static void counts() throws Exception {
        ThreadPoolExecutor p = pool(2, 2);
        CountDownLatch started = new CountDownLatch(2);
        CountDownLatch release = new CountDownLatch(1);
        for (int i = 0; i < 3; i++) {
            p.execute(() -> {
                started.countDown();
                try {
                    release.await();
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                }
            });
        }
        started.await();
        System.out.println("counts-busy: pool=" + p.getPoolSize() + " active=" + p.getActiveCount() + " queued="
                + p.getQueue().size() + " tasks=" + p.getTaskCount() + " completed=" + p.getCompletedTaskCount()
                + " state=" + state(p));
        release.countDown();
        p.shutdown();
        boolean terminated = p.awaitTermination(10, TimeUnit.SECONDS);
        // Interpreter round i5 wave 105, lane L6: termination is signalled by
        // the LAST worker's `tryTerminate`, while another worker that already
        // took itself off the count (`getTask`'s `decrementWorkerCount`) may
        // not have reached `processWorkerExit`'s `workers.remove` yet.
        // `getPoolSize()` answers 0 from TIDYING on, but `toString()` counts
        // `workers.size()`, so "pool size = 1" right after
        // `awaitTermination` is a JDK race (HotSpot can print it too; it raced
        // once on Linux). Wait for the straggler before printing.
        for (int i = 0; i < 500 && !state(p).contains("pool size = 0"); i++) {
            Thread.sleep(10);
        }
        System.out.println("counts-done: terminated=" + terminated + " pool=" + p.getPoolSize() + " largest="
                + p.getLargestPoolSize() + " tasks=" + p.getTaskCount() + " completed=" + p.getCompletedTaskCount()
                + " shutdown=" + p.isShutdown() + " terminating=" + p.isTerminating() + " state=" + state(p));
    }

    static void shutdownNow() throws Exception {
        ThreadPoolExecutor p = pool(1, 1);
        CountDownLatch started = new CountDownLatch(1);
        CountDownLatch never = new CountDownLatch(1);
        boolean[] interrupted = new boolean[1];
        p.execute(() -> {
            started.countDown();
            try {
                never.await();
            } catch (InterruptedException e) {
                interrupted[0] = true;
            }
        });
        started.await();
        p.execute(new Named(never));
        p.execute(new Named(never));
        List<Runnable> drained = p.shutdownNow();
        StringBuilder kinds = new StringBuilder();
        for (Runnable r : drained) {
            if (kinds.length() > 0) {
                kinds.append(',');
            }
            kinds.append(r.getClass().getName());
        }
        boolean terminated = p.awaitTermination(10, TimeUnit.SECONDS);
        System.out.println("shutdown-now: drained=" + drained.size() + " kinds=" + kinds + " interrupted="
                + interrupted[0] + " shutdown=" + p.isShutdown() + " terminated=" + (terminated && p.isTerminated()));
    }

    static void rejected() throws Exception {
        ThreadPoolExecutor p = pool(1, 1);
        p.shutdown();
        try {
            p.submit(() -> { });
            System.out.println("rejected: no exception");
        } catch (RuntimeException e) {
            System.out.println("rejected: " + e.getClass().getName() + " frames=" + frames(e.getStackTrace()));
        }
        ThreadPoolExecutor q = pool(1, 1);
        try {
            q.execute(null);
            System.out.println("rejected-null: no exception");
        } catch (RuntimeException e) {
            StackTraceElement[] st = e.getStackTrace();
            System.out.println("rejected-null: " + e.getClass().getName() + " top="
                    + (st.length == 0 ? "none" : simple(st[0])));
        }
        q.shutdown();
        q.awaitTermination(10, TimeUnit.SECONDS);
    }

    static void submitShapes() throws Exception {
        ThreadPoolExecutor p = pool(2, 2);
        Future<String> c = p.submit(() -> "c");
        Future<String> r = p.submit(() -> { }, "r");
        Future<?> n = p.submit(() -> { });
        List<Callable<String>> two = new ArrayList<>();
        two.add(() -> "a");
        two.add(() -> "b");
        List<String> all = new ArrayList<>();
        for (Future<String> f : p.invokeAll(two)) {
            all.add(f.get());
        }
        List<Callable<String>> any = new ArrayList<>();
        any.add(() -> {
            throw new IllegalStateException("no");
        });
        any.add(() -> "ok");
        String anyValue = p.invokeAny(any);
        System.out.println("submit-shapes: callable=" + c.get() + " runnable-result=" + r.get() + " runnable="
                + n.get() + " future=" + c.getClass().getName() + " all=" + all + " any=" + anyValue);
        p.shutdown();
        p.awaitTermination(10, TimeUnit.SECONDS);
    }

    static final class Hooked extends ThreadPoolExecutor {
        final List<String> hooks = java.util.Collections.synchronizedList(new ArrayList<String>());
        int isShutdownCalls;

        Hooked() {
            super(1, 1, 0L, TimeUnit.MILLISECONDS, new LinkedBlockingQueue<Runnable>());
        }

        @Override
        protected void beforeExecute(Thread t, Runnable r) {
            hooks.add("before");
        }

        @Override
        protected void afterExecute(Runnable r, Throwable t) {
            hooks.add("after");
        }

        @Override
        protected void terminated() {
            hooks.add("terminated");
        }

        @Override
        public boolean isShutdown() {
            isShutdownCalls++;
            return super.isShutdown();
        }

        @Override
        public int getPoolSize() {
            return super.getPoolSize() + 100;
        }
    }

    static void subclass() throws Exception {
        Hooked h = new Hooked();
        h.submit(() -> h.hooks.add("run")).get();
        int poolSize = h.getPoolSize();
        h.isShutdown();
        h.shutdown();
        h.isShutdown();
        h.awaitTermination(10, TimeUnit.SECONDS);
        System.out.println("subclass: hooks=" + String.join(",", h.hooks) + " isShutdownCalls=" + h.isShutdownCalls
                + " pool=" + poolSize + " core=" + h.getCorePoolSize());
    }

    static void reflect() throws Throwable {
        ThreadPoolExecutor p = pool(3, 4);
        Method core = ThreadPoolExecutor.class.getMethod("getCorePoolSize");
        MethodHandle max = MethodHandles.lookup().findVirtual(ThreadPoolExecutor.class, "getMaximumPoolSize",
                MethodType.methodType(int.class));
        Method shut = ThreadPoolExecutor.class.getMethod("isShutdown");
        MethodHandle term = MethodHandles.lookup().findVirtual(ThreadPoolExecutor.class, "isTerminated",
                MethodType.methodType(boolean.class));
        MethodHandle ifaceShut = MethodHandles.lookup().findVirtual(ExecutorService.class, "isShutdown",
                MethodType.methodType(boolean.class));
        int c = (Integer) core.invoke(p);
        int m = (int) max.invokeExact(p);
        boolean s = (Boolean) shut.invoke(p);
        boolean t = (boolean) term.invokeExact(p);
        boolean is = (boolean) ifaceShut.invoke((ExecutorService) p);
        System.out.println("reflect: core=" + c + " max=" + m + " shutdown=" + s + " terminated=" + t
                + " iface-shutdown=" + is);
        p.shutdown();
        p.awaitTermination(10, TimeUnit.SECONDS);
    }

    static volatile boolean mainAwaiting;

    static void awaitWaiter() throws Exception {
        ThreadPoolExecutor p = pool(1, 1);
        Thread main = Thread.currentThread();
        String[] seen = new String[3];
        p.execute(() -> {
            long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(10);
            while (!mainAwaiting && System.nanoTime() < deadline) {
                Thread.onSpinWait();
            }
            while (main.getState() != Thread.State.TIMED_WAITING && System.nanoTime() < deadline) {
                try {
                    Thread.sleep(1);
                } catch (InterruptedException e) {
                    return;
                }
            }
            Object blocker = LockSupport.getBlocker(main);
            seen[0] = String.valueOf(main.getState());
            seen[1] = blocker == null ? "null" : blocker.getClass().getName();
            seen[2] = frames(main.getStackTrace());
        });
        p.shutdown();
        mainAwaiting = true;
        boolean terminated = p.awaitTermination(30, TimeUnit.SECONDS);
        System.out.println("await-waiter: state=" + seen[0] + " blocker=" + seen[1] + " frames=" + seen[2]
                + " terminated=" + terminated);
    }

    static void scheduled() throws Exception {
        ScheduledThreadPoolExecutor zero = new ScheduledThreadPoolExecutor(0);
        int core0 = zero.getCorePoolSize();
        zero.shutdown();
        RejectedExecutionHandler handler = new ThreadPoolExecutor.DiscardPolicy();
        ScheduledThreadPoolExecutor s = new ScheduledThreadPoolExecutor(1, Executors.defaultThreadFactory(), handler);
        boolean sameHandler = s.getRejectedExecutionHandler() == handler;
        ScheduledFuture<String> f = s.schedule(() -> "s", 10, TimeUnit.MILLISECONDS);
        String value = f.get();
        s.shutdown();
        boolean terminated = s.awaitTermination(10, TimeUnit.SECONDS) && zero.awaitTermination(10, TimeUnit.SECONDS);
        System.out.println("scheduled: core0=" + core0 + " handler=" + sameHandler + " value=" + value
                + " terminated=" + terminated);
    }

    static void single() throws Exception {
        ExecutorService e = Executors.newSingleThreadExecutor();
        Object v = e.submit(() -> 1).get();
        System.out.println("single: class=" + e.getClass().getName() + " isTpe=" + (e instanceof ThreadPoolExecutor)
                + " value=" + v);
        e.shutdown();
        e.awaitTermination(10, TimeUnit.SECONDS);
    }

    interface Section {
        void run() throws Throwable;
    }

    static void section(String name, Section s) {
        try {
            s.run();
        } catch (Throwable t) {
            System.out.println(name + " failed: " + t);
        }
    }

    public static void main(String[] args) {
        section("worker-name", L1W82ThreadPoolsBodies::workerName);
        section("counts", L1W82ThreadPoolsBodies::counts);
        section("shutdown-now", L1W82ThreadPoolsBodies::shutdownNow);
        section("rejected", L1W82ThreadPoolsBodies::rejected);
        section("submit-shapes", L1W82ThreadPoolsBodies::submitShapes);
        section("subclass", L1W82ThreadPoolsBodies::subclass);
        section("reflect", L1W82ThreadPoolsBodies::reflect);
        section("await-waiter", L1W82ThreadPoolsBodies::awaitWaiter);
        section("scheduled", L1W82ThreadPoolsBodies::scheduled);
        section("single", L1W82ThreadPoolsBodies::single);
        System.out.println("done");
    }
}
