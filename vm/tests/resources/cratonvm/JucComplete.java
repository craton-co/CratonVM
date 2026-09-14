// JAVA21+
package cratonvm;

import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicReference;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.Semaphore;
import java.util.concurrent.CyclicBarrier;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.locks.ReentrantLock;
import java.util.concurrent.locks.ReentrantReadWriteLock;
import java.util.concurrent.locks.Condition;
import java.util.concurrent.CompletableFuture;

/**
 * Session 49: TCK — java.util.concurrent conformance tests.
 *
 * Each test method returns an int:
 *   positive = success (expected value)
 *   0 = failure
 *
 * Tests cover: Atomics, Locks, Synchronizers, Concurrent Collections,
 * Blocking Queues, and CompletableFuture.
 */
public class JucComplete {

    // ========================================================================
    // ATOMICS
    // ========================================================================

    // 1: AtomicInteger compareAndSet
    public static int testAtomicIntCas() {
        AtomicInteger ai = new AtomicInteger(10);
        boolean ok1 = ai.compareAndSet(10, 20);
        boolean ok2 = ai.compareAndSet(10, 30); // should fail
        return (ok1 && !ok2 && ai.get() == 20) ? 1 : 0;
    }

    // 2: AtomicInteger getAndIncrement/getAndDecrement
    public static int testAtomicIntIncrDecr() {
        AtomicInteger ai = new AtomicInteger(5);
        int old1 = ai.getAndIncrement(); // returns 5, now 6
        int old2 = ai.getAndDecrement(); // returns 6, now 5
        return (old1 == 5 && old2 == 6 && ai.get() == 5) ? 1 : 0;
    }

    // 3: AtomicInteger incrementAndGet/decrementAndGet
    public static int testAtomicIntPreIncrDecr() {
        AtomicInteger ai = new AtomicInteger(0);
        int v1 = ai.incrementAndGet(); // 1
        int v2 = ai.incrementAndGet(); // 2
        int v3 = ai.decrementAndGet(); // 1
        return (v1 == 1 && v2 == 2 && v3 == 1) ? 1 : 0;
    }

    // 4: AtomicInteger getAndAdd/addAndGet
    public static int testAtomicIntAddOps() {
        AtomicInteger ai = new AtomicInteger(10);
        int old = ai.getAndAdd(5); // returns 10, now 15
        int cur = ai.addAndGet(3); // 18
        return (old == 10 && cur == 18 && ai.get() == 18) ? 1 : 0;
    }

    // 5: AtomicInteger getAndSet
    public static int testAtomicIntGetAndSet() {
        AtomicInteger ai = new AtomicInteger(42);
        int old = ai.getAndSet(99);
        return (old == 42 && ai.get() == 99) ? 1 : 0;
    }

    // 6: AtomicLong basic operations
    public static int testAtomicLongBasic() {
        AtomicLong al = new AtomicLong(100L);
        boolean ok = al.compareAndSet(100L, 200L);
        long old = al.getAndAdd(50L); // returns 200, now 250
        long cur = al.incrementAndGet(); // 251
        return (ok && old == 200L && cur == 251L) ? 1 : 0;
    }

    // 7: AtomicBoolean compareAndSet
    public static int testAtomicBooleanCas() {
        AtomicBoolean ab = new AtomicBoolean(false);
        boolean ok1 = ab.compareAndSet(false, true);
        boolean ok2 = ab.compareAndSet(false, true); // should fail
        return (ok1 && !ok2 && ab.get()) ? 1 : 0;
    }

    // 8: AtomicBoolean getAndSet
    public static int testAtomicBooleanGetAndSet() {
        AtomicBoolean ab = new AtomicBoolean(true);
        boolean old = ab.getAndSet(false);
        return (old && !ab.get()) ? 1 : 0;
    }

    // 9: AtomicReference compareAndSet
    public static int testAtomicRefCas() {
        AtomicReference<String> ar = new AtomicReference<>("hello");
        boolean ok = ar.compareAndSet("hello", "world");
        String val = ar.get();
        return (ok && "world".equals(val)) ? 1 : 0;
    }

    // 10: AtomicReference getAndSet
    public static int testAtomicRefGetAndSet() {
        AtomicReference<String> ar = new AtomicReference<>("A");
        String old = (String) ar.getAndSet("B");
        return ("A".equals(old) && "B".equals(ar.get())) ? 1 : 0;
    }

    // 11: AtomicInteger concurrent increment (multi-threaded)
    public static int testAtomicIntConcurrentIncr() {
        AtomicInteger ai = new AtomicInteger(0);
        Thread[] threads = new Thread[5];
        for (int i = 0; i < 5; i++) {
            threads[i] = new Thread(() -> {
                for (int j = 0; j < 20; j++) {
                    ai.incrementAndGet();
                }
            });
        }
        for (Thread t : threads) t.start();
        try {
            for (Thread t : threads) t.join();
        } catch (InterruptedException e) { return 0; }
        return ai.get(); // 100
    }

    // ========================================================================
    // REENTRANT LOCK
    // ========================================================================

    // 12: ReentrantLock basic lock/unlock
    public static int testReentrantLockBasic() {
        ReentrantLock lock = new ReentrantLock();
        lock.lock();
        try {
            return lock.isLocked() ? 1 : 0;
        } finally {
            lock.unlock();
        }
    }

    // 13: ReentrantLock tryLock
    public static int testReentrantLockTryLock() {
        ReentrantLock lock = new ReentrantLock();
        boolean got = lock.tryLock();
        if (!got) return 0;
        try {
            return lock.isHeldByCurrentThread() ? 1 : 0;
        } finally {
            lock.unlock();
        }
    }

    // 14: ReentrantLock reentrant
    public static int testReentrantLockReentrant() {
        ReentrantLock lock = new ReentrantLock();
        lock.lock();
        lock.lock(); // reentrant
        int holdCount = lock.getHoldCount();
        lock.unlock();
        lock.unlock();
        return (holdCount == 2 && !lock.isLocked()) ? 1 : 0;
    }

    // 15: ReentrantLock with Condition signal
    public static int testReentrantLockCondition() {
        ReentrantLock lock = new ReentrantLock();
        Condition cond = lock.newCondition();
        // Just verify we can create a Condition and signal it
        lock.lock();
        try {
            cond.signal();
            return 1;
        } finally {
            lock.unlock();
        }
    }

    // 16: ReentrantReadWriteLock basic
    public static int testReadWriteLockBasic() {
        ReentrantReadWriteLock rwl = new ReentrantReadWriteLock();
        rwl.readLock().lock();
        rwl.readLock().unlock();
        rwl.writeLock().lock();
        rwl.writeLock().unlock();
        return 1;
    }

    // ========================================================================
    // COUNTDOWN LATCH
    // ========================================================================

    // 17: CountDownLatch countDown + await (count reaches zero before await)
    public static int testCountDownLatchBasic() {
        CountDownLatch latch = new CountDownLatch(2);
        latch.countDown();
        latch.countDown();
        try {
            latch.await(); // should return immediately
        } catch (InterruptedException e) { return 0; }
        return (latch.getCount() == 0) ? 1 : 0;
    }

    // 18: CountDownLatch getCount decreases
    public static int testCountDownLatchGetCount() {
        CountDownLatch latch = new CountDownLatch(3);
        int c1 = (int) latch.getCount(); // 3
        latch.countDown();
        int c2 = (int) latch.getCount(); // 2
        latch.countDown();
        latch.countDown();
        int c3 = (int) latch.getCount(); // 0
        return (c1 == 3 && c2 == 2 && c3 == 0) ? 1 : 0;
    }

    // 19: CountDownLatch extra countDown below zero stays at zero
    public static int testCountDownLatchExtraCountDown() {
        CountDownLatch latch = new CountDownLatch(1);
        latch.countDown();
        latch.countDown(); // extra
        latch.countDown(); // extra
        return (latch.getCount() == 0) ? 1 : 0;
    }

    // 20: CountDownLatch toString contains count
    public static int testCountDownLatchToString() {
        CountDownLatch latch = new CountDownLatch(5);
        String s = latch.toString();
        return (s != null && s.contains("5")) ? 1 : 0;
    }

    // 21: CountDownLatch await with timeout
    public static int testCountDownLatchAwaitTimeout() {
        CountDownLatch latch = new CountDownLatch(1);
        latch.countDown(); // count=0
        try {
            boolean ok = latch.await(1, TimeUnit.SECONDS);
            return ok ? 1 : 0;
        } catch (InterruptedException e) { return 0; }
    }

    // ========================================================================
    // SEMAPHORE
    // ========================================================================

    // 22: Semaphore acquire/release
    public static int testSemaphoreBasic() {
        Semaphore sem = new Semaphore(3);
        try {
            sem.acquire();
        } catch (InterruptedException e) { return 0; }
        int after = sem.availablePermits(); // 2
        sem.release();
        int afterRelease = sem.availablePermits(); // 3
        return (after == 2 && afterRelease == 3) ? 1 : 0;
    }

    // 23: Semaphore tryAcquire
    public static int testSemaphoreTryAcquire() {
        Semaphore sem = new Semaphore(1);
        boolean ok = sem.tryAcquire();
        boolean fail = sem.tryAcquire(); // no permits left
        sem.release();
        return (ok && !fail) ? 1 : 0;
    }

    // 24: Semaphore drainPermits
    public static int testSemaphoreDrain() {
        Semaphore sem = new Semaphore(5);
        int drained = sem.drainPermits();
        return (drained == 5 && sem.availablePermits() == 0) ? 1 : 0;
    }

    // 25: Semaphore release above initial
    public static int testSemaphoreReleaseAboveInit() {
        Semaphore sem = new Semaphore(2);
        sem.release(); // 3
        sem.release(); // 4
        return (sem.availablePermits() == 4) ? 1 : 0;
    }

    // 26: Semaphore acquire(n)
    public static int testSemaphoreAcquireN() {
        Semaphore sem = new Semaphore(10);
        try {
            sem.acquire(3);
        } catch (InterruptedException e) { return 0; }
        int remaining = sem.availablePermits();
        sem.release(3);
        return (remaining == 7 && sem.availablePermits() == 10) ? 1 : 0;
    }

    // Hikari's SuspendResumeLock drains a fair semaphore with this overload.
    // Keep it distinct from acquire(int), which is interruptible.
    public static int testSemaphoreAcquireUninterruptiblyN() {
        Semaphore sem = new Semaphore(2, true);
        sem.acquireUninterruptibly(2);
        int remaining = sem.availablePermits();
        sem.release(2);
        return (remaining == 0 && sem.availablePermits() == 2) ? 1 : 0;
    }

    // 27: Semaphore isFair
    public static int testSemaphoreIsFair() {
        Semaphore fair = new Semaphore(1, true);
        Semaphore unfair = new Semaphore(1);
        return (fair.isFair() && !unfair.isFair()) ? 1 : 0;
    }

    // ========================================================================
    // CYCLIC BARRIER
    // ========================================================================

    // 28: CyclicBarrier getParties
    public static int testCyclicBarrierGetParties() {
        CyclicBarrier cb = new CyclicBarrier(3);
        return (cb.getParties() == 3) ? 1 : 0;
    }

    // 29: CyclicBarrier isBroken (initially false)
    public static int testCyclicBarrierIsBroken() {
        CyclicBarrier cb = new CyclicBarrier(2);
        return (!cb.isBroken()) ? 1 : 0;
    }

    // 30: CyclicBarrier getNumberWaiting (initially 0)
    public static int testCyclicBarrierGetNumberWaiting() {
        CyclicBarrier cb = new CyclicBarrier(5);
        return (cb.getNumberWaiting() == 0) ? 1 : 0;
    }

    // 31: CyclicBarrier reset
    public static int testCyclicBarrierReset() {
        CyclicBarrier cb = new CyclicBarrier(2);
        cb.reset();
        return (!cb.isBroken() && cb.getNumberWaiting() == 0) ? 1 : 0;
    }

    // ========================================================================
    // CONCURRENT HASHMAP
    // ========================================================================

    // 32: ConcurrentHashMap put and get
    public static int testConcurrentHashMapPutGet() {
        ConcurrentHashMap<String, Integer> map = new ConcurrentHashMap<>();
        map.put("a", 1);
        map.put("b", 2);
        Integer v = map.get("a");
        return (v != null && v == 1 && map.size() == 2) ? 1 : 0;
    }

    // 33: ConcurrentHashMap containsKey
    public static int testConcurrentHashMapContainsKey() {
        ConcurrentHashMap<String, String> map = new ConcurrentHashMap<>();
        map.put("key", "value");
        return (map.containsKey("key") && !map.containsKey("missing")) ? 1 : 0;
    }

    // 34: ConcurrentHashMap remove
    public static int testConcurrentHashMapRemove() {
        ConcurrentHashMap<String, Integer> map = new ConcurrentHashMap<>();
        map.put("x", 42);
        Integer removed = map.remove("x");
        return (removed != null && removed == 42 && !map.containsKey("x")) ? 1 : 0;
    }

    // 35: ConcurrentHashMap putIfAbsent
    public static int testConcurrentHashMapPutIfAbsent() {
        ConcurrentHashMap<String, Integer> map = new ConcurrentHashMap<>();
        Integer first = map.putIfAbsent("key", 10);
        Integer second = map.putIfAbsent("key", 20);
        return (first == null && second != null && second == 10 && map.get("key") == 10) ? 1 : 0;
    }

    // 36: ConcurrentHashMap isEmpty
    public static int testConcurrentHashMapIsEmpty() {
        ConcurrentHashMap<String, String> map = new ConcurrentHashMap<>();
        boolean empty = map.isEmpty();
        map.put("k", "v");
        boolean notEmpty = !map.isEmpty();
        return (empty && notEmpty) ? 1 : 0;
    }

    // 37: ConcurrentHashMap getOrDefault
    public static int testConcurrentHashMapGetOrDefault() {
        ConcurrentHashMap<String, Integer> map = new ConcurrentHashMap<>();
        map.put("a", 1);
        int v1 = map.getOrDefault("a", 99); // 1
        int v2 = map.getOrDefault("b", 99); // 99
        return (v1 == 1 && v2 == 99) ? 1 : 0;
    }

    // ========================================================================
    // COPY ON WRITE ARRAY LIST
    // ========================================================================

    // 38: CopyOnWriteArrayList add and get
    public static int testCOWALAddGet() {
        CopyOnWriteArrayList<String> list = new CopyOnWriteArrayList<>();
        list.add("hello");
        list.add("world");
        return (list.size() == 2 && "hello".equals(list.get(0))) ? 1 : 0;
    }

    // 39: CopyOnWriteArrayList contains
    public static int testCOWALContains() {
        CopyOnWriteArrayList<String> list = new CopyOnWriteArrayList<>();
        list.add("abc");
        return (list.contains("abc") && !list.contains("xyz")) ? 1 : 0;
    }

    // 40: CopyOnWriteArrayList remove
    public static int testCOWALRemove() {
        CopyOnWriteArrayList<String> list = new CopyOnWriteArrayList<>();
        list.add("first");
        list.add("second");
        boolean removed = list.remove("first");
        return (removed && list.size() == 1 && "second".equals(list.get(0))) ? 1 : 0;
    }

    // 41: CopyOnWriteArrayList isEmpty
    public static int testCOWALIsEmpty() {
        CopyOnWriteArrayList<String> list = new CopyOnWriteArrayList<>();
        boolean empty = list.isEmpty();
        list.add("x");
        boolean notEmpty = !list.isEmpty();
        return (empty && notEmpty) ? 1 : 0;
    }

    // ========================================================================
    // LINKED BLOCKING QUEUE
    // ========================================================================

    // 42: LinkedBlockingQueue offer and poll
    public static int testLinkedBlockingQueueOfferPoll() {
        LinkedBlockingQueue<Integer> q = new LinkedBlockingQueue<>();
        q.offer(10);
        q.offer(20);
        Integer v = q.poll();
        return (v != null && v == 10 && q.size() == 1) ? 1 : 0;
    }

    // 43: LinkedBlockingQueue put and take (non-blocking path)
    public static int testLinkedBlockingQueuePutTake() {
        LinkedBlockingQueue<String> q = new LinkedBlockingQueue<>();
        try {
            q.put("hello");
            String v = q.take();
            return "hello".equals(v) ? 1 : 0;
        } catch (InterruptedException e) { return 0; }
    }

    // 44: LinkedBlockingQueue peek
    public static int testLinkedBlockingQueuePeek() {
        LinkedBlockingQueue<Integer> q = new LinkedBlockingQueue<>();
        Integer empty = q.peek();
        q.offer(42);
        Integer val = q.peek();
        return (empty == null && val != null && val == 42 && q.size() == 1) ? 1 : 0;
    }

    // 45: LinkedBlockingQueue isEmpty/size
    public static int testLinkedBlockingQueueIsEmptySize() {
        LinkedBlockingQueue<String> q = new LinkedBlockingQueue<>();
        boolean empty = q.isEmpty();
        q.offer("a");
        q.offer("b");
        return (empty && q.size() == 2 && !q.isEmpty()) ? 1 : 0;
    }

    // 46: LinkedBlockingQueue with capacity
    public static int testLinkedBlockingQueueCapacity() {
        LinkedBlockingQueue<Integer> q = new LinkedBlockingQueue<>(2);
        boolean ok1 = q.offer(1);
        boolean ok2 = q.offer(2);
        boolean ok3 = q.offer(3); // should fail - full
        return (ok1 && ok2 && !ok3 && q.size() == 2) ? 1 : 0;
    }

    // ========================================================================
    // ARRAY BLOCKING QUEUE
    // ========================================================================

    // 47: ArrayBlockingQueue offer and poll
    public static int testArrayBlockingQueueOfferPoll() {
        ArrayBlockingQueue<Integer> q = new ArrayBlockingQueue<>(5);
        q.offer(100);
        q.offer(200);
        Integer v = q.poll();
        return (v != null && v == 100 && q.size() == 1) ? 1 : 0;
    }

    // 48: ArrayBlockingQueue capacity enforcement
    public static int testArrayBlockingQueueCapacity() {
        ArrayBlockingQueue<Integer> q = new ArrayBlockingQueue<>(2);
        boolean ok1 = q.offer(1);
        boolean ok2 = q.offer(2);
        boolean ok3 = q.offer(3); // should fail
        return (ok1 && ok2 && !ok3) ? 1 : 0;
    }

    // 49: ArrayBlockingQueue remainingCapacity
    public static int testArrayBlockingQueueRemainingCapacity() {
        ArrayBlockingQueue<String> q = new ArrayBlockingQueue<>(5);
        q.offer("x");
        int remaining = q.remainingCapacity();
        return (remaining == 4) ? 1 : 0;
    }

    // ========================================================================
    // COMPLETABLE FUTURE
    // ========================================================================

    // 50: CompletableFuture complete and get
    public static int testCompletableFutureComplete() {
        CompletableFuture<Integer> cf = new CompletableFuture<>();
        cf.complete(42);
        try {
            Integer v = cf.get();
            return (v != null && v == 42 && cf.isDone()) ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    // 51: CompletableFuture completedFuture
    public static int testCompletableFutureCompletedFuture() {
        CompletableFuture<String> cf = CompletableFuture.completedFuture("done");
        try {
            return ("done".equals(cf.get()) && cf.isDone()) ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    // 52: CompletableFuture thenApply
    public static int testCompletableFutureThenApply() {
        CompletableFuture<Integer> cf = CompletableFuture.completedFuture(10);
        CompletableFuture<Integer> result = cf.thenApply(x -> x * 2);
        try {
            return (result.get() == 20) ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    // 53: CompletableFuture thenAccept
    public static int testCompletableFutureThenAccept() {
        CompletableFuture<String> cf = CompletableFuture.completedFuture("test");
        final int[] seen = {0};
        cf.thenAccept(s -> { seen[0] = s.length(); });
        return (seen[0] == 4) ? 1 : 0;
    }

    // 54: CompletableFuture isCancelled/isDone
    public static int testCompletableFutureState() {
        CompletableFuture<String> cf = new CompletableFuture<>();
        boolean notDone = !cf.isDone();
        boolean notCancelled = !cf.isCancelled();
        cf.complete("ok");
        boolean done = cf.isDone();
        return (notDone && notCancelled && done) ? 1 : 0;
    }

    // 55: CompletableFuture cancel
    public static int testCompletableFutureCancel() {
        CompletableFuture<String> cf = new CompletableFuture<>();
        boolean cancelled = cf.cancel(true);
        return (cancelled && cf.isCancelled() && cf.isDone()) ? 1 : 0;
    }

    // 56: CompletableFuture exceptionally
    public static int testCompletableFutureExceptionally() {
        CompletableFuture<Integer> cf = new CompletableFuture<>();
        cf.completeExceptionally(new RuntimeException("fail"));
        CompletableFuture<Integer> recovery = cf.exceptionally(ex -> -1);
        try {
            return (recovery.get() == -1) ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    // 57: CompletableFuture isCompletedExceptionally
    public static int testCompletableFutureIsCompletedExceptionally() {
        CompletableFuture<String> cf = new CompletableFuture<>();
        cf.completeExceptionally(new RuntimeException("oops"));
        return cf.isCompletedExceptionally() ? 1 : 0;
    }

    // ========================================================================
    // MULTI-THREADED SYNCHRONIZER TESTS
    // ========================================================================

    // 58: CountDownLatch with threads
    public static int testCountDownLatchThreaded() {
        CountDownLatch latch = new CountDownLatch(3);
        AtomicInteger sum = new AtomicInteger(0);
        for (int i = 0; i < 3; i++) {
            final int val = i + 1;
            new Thread(() -> {
                sum.addAndGet(val);
                latch.countDown();
            }).start();
        }
        try {
            latch.await();
        } catch (InterruptedException e) { return 0; }
        return sum.get(); // 1+2+3 = 6
    }

    // 59: Semaphore with threads
    public static int testSemaphoreThreaded() {
        Semaphore sem = new Semaphore(1);
        AtomicInteger counter = new AtomicInteger(0);
        Thread[] threads = new Thread[3];
        for (int i = 0; i < 3; i++) {
            threads[i] = new Thread(() -> {
                try {
                    sem.acquire();
                    counter.incrementAndGet();
                    sem.release();
                } catch (InterruptedException e) {}
            });
        }
        for (Thread t : threads) t.start();
        try {
            for (Thread t : threads) t.join();
        } catch (InterruptedException e) { return 0; }
        return counter.get(); // 3
    }

    // 60: ReentrantLock protecting shared state
    public static int testReentrantLockThreaded() {
        ReentrantLock lock = new ReentrantLock();
        final int[] shared = {0};
        Thread[] threads = new Thread[4];
        for (int i = 0; i < 4; i++) {
            threads[i] = new Thread(() -> {
                for (int j = 0; j < 25; j++) {
                    lock.lock();
                    try {
                        shared[0]++;
                    } finally {
                        lock.unlock();
                    }
                }
            });
        }
        for (Thread t : threads) t.start();
        try {
            for (Thread t : threads) t.join();
        } catch (InterruptedException e) { return 0; }
        return shared[0]; // 100
    }

    // 61: ConcurrentHashMap concurrent put from threads
    public static int testConcurrentHashMapThreaded() {
        ConcurrentHashMap<Integer, Integer> map = new ConcurrentHashMap<>();
        Thread[] threads = new Thread[5];
        for (int i = 0; i < 5; i++) {
            final int start = i * 10;
            threads[i] = new Thread(() -> {
                for (int j = 0; j < 10; j++) {
                    map.put(start + j, start + j);
                }
            });
        }
        for (Thread t : threads) t.start();
        try {
            for (Thread t : threads) t.join();
        } catch (InterruptedException e) { return 0; }
        return map.size(); // 50
    }

    // 62: LinkedBlockingQueue producer-consumer
    public static int testBlockingQueueProducerConsumer() {
        LinkedBlockingQueue<Integer> q = new LinkedBlockingQueue<>();
        AtomicInteger sum = new AtomicInteger(0);
        Thread producer = new Thread(() -> {
            for (int i = 1; i <= 5; i++) {
                try { q.put(i); } catch (InterruptedException e) {}
            }
        });
        Thread consumer = new Thread(() -> {
            for (int i = 0; i < 5; i++) {
                try {
                    Integer v = q.take();
                    sum.addAndGet(v);
                } catch (InterruptedException e) {}
            }
        });
        producer.start();
        consumer.start();
        try {
            producer.join();
            consumer.join();
        } catch (InterruptedException e) { return 0; }
        return sum.get(); // 1+2+3+4+5 = 15
    }

    // 63: CopyOnWriteArrayList thread safety
    public static int testCOWALThreaded() {
        CopyOnWriteArrayList<Integer> list = new CopyOnWriteArrayList<>();
        Thread[] threads = new Thread[4];
        for (int i = 0; i < 4; i++) {
            final int val = i;
            threads[i] = new Thread(() -> {
                list.add(val);
            });
        }
        for (Thread t : threads) t.start();
        try {
            for (Thread t : threads) t.join();
        } catch (InterruptedException e) { return 0; }
        return list.size(); // 4
    }

    // 64: AtomicInteger lazySet + get
    public static int testAtomicIntLazySet() {
        AtomicInteger ai = new AtomicInteger(0);
        ai.lazySet(99);
        return ai.get(); // 99
    }

    // 65: AtomicLong lazySet + get
    public static int testAtomicLongLazySet() {
        AtomicLong al = new AtomicLong(0L);
        al.lazySet(12345L);
        // Return high-bits test: 12345L fits in int
        return (al.get() == 12345L) ? 1 : 0;
    }

    // 66: ConcurrentHashMap replace
    public static int testConcurrentHashMapReplace() {
        ConcurrentHashMap<String, Integer> map = new ConcurrentHashMap<>();
        map.put("key", 10);
        Integer old = map.replace("key", 20);
        Integer missing = map.replace("nonexistent", 30);
        return (old != null && old == 10 && map.get("key") == 20 && missing == null) ? 1 : 0;
    }

    // 67: ConcurrentHashMap containsValue
    public static int testConcurrentHashMapContainsValue() {
        ConcurrentHashMap<String, Integer> map = new ConcurrentHashMap<>();
        map.put("a", 100);
        map.put("b", 200);
        return (map.containsValue(100) && !map.containsValue(999)) ? 1 : 0;
    }

    // 68: Multiple synchronizers in sequence
    public static int testSynchronizerComposition() {
        AtomicInteger result = new AtomicInteger(0);
        CountDownLatch ready = new CountDownLatch(1);
        Semaphore gate = new Semaphore(0);

        new Thread(() -> {
            result.set(10);
            ready.countDown();  // signal ready
            gate.release();     // open gate
        }).start();

        try {
            ready.await();
            gate.acquire();
        } catch (InterruptedException e) { return 0; }

        return result.get(); // 10
    }

    // 69: LinkedBlockingQueue clear
    public static int testLinkedBlockingQueueClear() {
        LinkedBlockingQueue<Integer> q = new LinkedBlockingQueue<>();
        q.offer(1);
        q.offer(2);
        q.offer(3);
        q.clear();
        return (q.isEmpty() && q.size() == 0) ? 1 : 0;
    }

    // 70: ConcurrentHashMap clear
    public static int testConcurrentHashMapClear() {
        ConcurrentHashMap<String, String> map = new ConcurrentHashMap<>();
        map.put("a", "1");
        map.put("b", "2");
        map.clear();
        return (map.isEmpty() && map.size() == 0) ? 1 : 0;
    }
}
