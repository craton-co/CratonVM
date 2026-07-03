import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;
import java.util.concurrent.locks.Condition;
import java.util.concurrent.locks.ReentrantLock;

/**
 * Self-contained mirror of Spring's ConcurrencyThrottleInterceptorTests
 * (no Spring classpath needed): a JDK dynamic proxy whose InvocationHandler
 * applies a ReentrantLock+Condition concurrency throttle, hammered by
 * NR_OF_THREADS threads x NR_OF_ITERATIONS proxied calls, plus
 * NR_OF_THREADS/10 threads that throw through the same interceptor chain.
 */
public class MiniThrottle {
    static final int NR_OF_THREADS = 100;
    static final int NR_OF_ITERATIONS = 1000;

    interface ITestBean {
        String getName();
        void exceptional(Throwable t) throws Throwable;
    }

    static class TestBean implements ITestBean {
        private final String name = "target";
        public String getName() { return name; }
        public void exceptional(Throwable t) throws Throwable { if (t != null) throw t; }
    }

    static class Throttle {
        final ReentrantLock lock = new ReentrantLock();
        final Condition cond = lock.newCondition();
        final int limit;
        int count;
        Throttle(int limit) { this.limit = limit; }
        void beforeAccess() {
            lock.lock();
            try {
                while (count >= limit) {
                    cond.awaitUninterruptibly();
                }
                count++;
            } finally {
                lock.unlock();
            }
        }
        void afterAccess() {
            lock.lock();
            try {
                count--;
                cond.signal();
            } finally {
                lock.unlock();
            }
        }
    }

    static ITestBean proxyFor(TestBean target, Throttle throttle) {
        InvocationHandler h = new InvocationHandler() {
            public Object invoke(Object proxy, Method method, Object[] args) throws Throwable {
                throttle.beforeAccess();
                try {
                    return method.invoke(target, args);
                } catch (java.lang.reflect.InvocationTargetException ite) {
                    throw ite.getTargetException();
                } finally {
                    throttle.afterAccess();
                }
            }
        };
        return (ITestBean) Proxy.newProxyInstance(
                MiniThrottle.class.getClassLoader(), new Class<?>[]{ITestBean.class}, h);
    }

    static void runPass(int limit) throws Exception {
        TestBean tb = new TestBean();
        Throttle throttle = new Throttle(limit);
        ITestBean proxy = proxyFor(tb, throttle);

        Thread[] threads = new Thread[NR_OF_THREADS];
        for (int i = 0; i < NR_OF_THREADS; i++) {
            threads[i] = new Worker(proxy, null);
            threads[i].start();
        }
        for (int i = 0; i < NR_OF_THREADS / 10; i++) {
            Thread.sleep(5);
            threads[i] = new Worker(proxy,
                    (i % 2 == 0 ? new OutOfMemoryError() : new IllegalStateException()));
            threads[i].start();
        }
        for (int i = 0; i < NR_OF_THREADS; i++) {
            threads[i].join();
        }
        System.out.println("PASS limit=" + limit + " done, residual count=" + throttle.count);
    }

    static class Worker extends Thread {
        final ITestBean proxy;
        final Throwable ex;
        Worker(ITestBean proxy, Throwable ex) { this.proxy = proxy; this.ex = ex; }
        public void run() {
            if (ex != null) {
                try {
                    proxy.exceptional(ex);
                } catch (RuntimeException | Error err) {
                    if (err != ex) err.printStackTrace();
                } catch (Throwable t) {
                    t.printStackTrace();
                }
            } else {
                for (int i = 0; i < NR_OF_ITERATIONS; i++) {
                    proxy.getName();
                }
            }
        }
    }

    public static void main(String[] args) throws Exception {
        long t0 = System.nanoTime();
        runPass(1);
        runPass(10);
        long ms = (System.nanoTime() - t0) / 1_000_000;
        System.out.println("ALL THREADS JOINED ms=" + ms);
    }
}
