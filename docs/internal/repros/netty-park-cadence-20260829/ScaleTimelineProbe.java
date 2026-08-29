package io.netty.util.concurrent;

import io.netty.util.concurrent.AutoScalingEventExecutorChooserFactory.AutoScalingUtilizationMetric;

import java.util.List;
import java.util.concurrent.Executor;
import java.util.concurrent.Executors;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;

/**
 * Timeline probe for AutoScalingEventExecutorChooserFactoryTest.testScaleUp.
 * Mirrors the test's executor/group shapes exactly, but instead of the test's
 * 50ms polling loop it samples every 2ms and prints the whole timeline, so the
 * 1 -> 2 -> 3 transitions and the per-executor utilizations are visible.
 */
public final class ScaleTimelineProbe {

    private static void busyTask(long duration, TimeUnit unit) {
        long endTime = System.nanoTime() + unit.toNanos(duration);
        while (System.nanoTime() < endTime) { }
    }

    static final class TestEventExecutor extends SingleThreadEventExecutor {
        private final AtomicBoolean highLoad = new AtomicBoolean(false);
        final int id;

        TestEventExecutor(EventExecutorGroup parent, Executor executor, int id) {
            super(parent, executor, true, true, DEFAULT_MAX_PENDING_EXECUTOR_TASKS,
                  RejectedExecutionHandlers.reject());
            this.id = id;
        }

        void setHighLoad(boolean highLoad) { this.highLoad.set(highLoad); }

        @Override
        protected void run() {
            do {
                if (highLoad.get()) {
                    runAllTasks(TimeUnit.MILLISECONDS.toNanos(20));
                    long busyWorkStart = ticker().nanoTime();
                    busyTask(35, TimeUnit.MILLISECONDS);
                    long busyWorkEnd = ticker().nanoTime();
                    reportActiveIoTime(busyWorkEnd - busyWorkStart);
                    try {
                        Thread.sleep(10);
                    } catch (InterruptedException e) {
                        Thread.currentThread().interrupt();
                        break;
                    }
                } else {
                    boolean ranTask = runAllTasks();
                    if (ranTask) { updateLastExecutionTime(); continue; }
                    try {
                        Thread.sleep(50);
                    } catch (InterruptedException e) {
                        Thread.currentThread().interrupt();
                        break;
                    }
                }
            } while (!confirmShutdown() && !canSuspend());
        }
    }

    static final class TestEventExecutorGroup extends MultithreadEventExecutorGroup {
        private static final Object[] ARGS = new Object[0];
        private int next;

        TestEventExecutorGroup(int minThreads, int maxThreads, long checkPeriod, TimeUnit unit) {
            super(maxThreads,
                  new ThreadPerTaskExecutor(Executors.defaultThreadFactory()),
                  new AutoScalingEventExecutorChooserFactory(
                          minThreads, maxThreads, checkPeriod, unit, 0.4, 0.6,
                          maxThreads, maxThreads, 2),
                  ARGS);
        }

        @Override
        protected EventExecutor newChild(Executor executor, Object... args) {
            return new TestEventExecutor(this, executor, next++);
        }
    }

    static long t0;
    static double ms() { return (System.nanoTime() - t0) / 1e6; }

    public static void main(String[] args) throws Exception {
        t0 = System.nanoTime();
        TestEventExecutorGroup group = new TestEventExecutorGroup(1, 3, 50, TimeUnit.MILLISECONDS);
        try {
            java.util.concurrent.CountDownLatch startLatch = new java.util.concurrent.CountDownLatch(group.executorCount());
            for (EventExecutor exec : group) { exec.execute(startLatch::countDown); }
            startLatch.await();
            System.out.printf("[%8.2f] all started, active=%d%n", ms(), group.activeExecutorCount());
            Thread.sleep(200);
            System.out.printf("[%8.2f] after 200ms, active=%d%n", ms(), group.activeExecutorCount());

            TestEventExecutor activeExecutor = null;
            for (EventExecutor exec : group) {
                if (!exec.isSuspended()) { activeExecutor = (TestEventExecutor) exec; break; }
            }
            if (activeExecutor == null) { System.out.println("NO ACTIVE EXECUTOR"); return; }
            System.out.printf("[%8.2f] stressing executor id=%d%n", ms(), activeExecutor.id);
            activeExecutor.setHighLoad(true);

            List<AutoScalingUtilizationMetric> metrics = group.executorUtilizations();
            int lastCount = group.activeExecutorCount();
            long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(3);
            int firstTwoSeen = -1;
            while (System.nanoTime() < deadline) {
                int c = group.activeExecutorCount();
                if (c != lastCount) {
                    StringBuilder sb = new StringBuilder();
                    for (int i = 0; i < metrics.size(); i++) {
                        AutoScalingUtilizationMetric m = metrics.get(i);
                        TestEventExecutor e = (TestEventExecutor) m.executor();
                        sb.append(String.format("  e%d{u=%.3f susp=%b}", e.id, m.utilization(), e.isSuspended()));
                    }
                    System.out.printf("[%8.2f] active %d -> %d %s%n", ms(), lastCount, c, sb);
                    lastCount = c;
                    if (c >= 2 && firstTwoSeen < 0) { firstTwoSeen = c; }
                }
                Thread.sleep(1);
            }
            System.out.printf("[%8.2f] END active=%d  (first count observed >= 2 was %d)%n",
                              ms(), group.activeExecutorCount(), firstTwoSeen);
        } finally {
            group.shutdownGracefully().syncUninterruptibly();
        }
    }
}
