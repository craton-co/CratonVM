import java.util.HashMap;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.locks.ReentrantLock;
import java.util.concurrent.locks.ReentrantReadWriteLock;

/**
 * Per-operation cost of the JDK primitives a Hibernate session open/close leans
 * on, measured at one thread and at N threads.
 *
 * <p>The point of the two-column shape is that "slow" and "does not scale" need
 * different fixes and look the same in an end-to-end number. A primitive whose
 * 1-thread and N-thread per-op costs match is merely slow — the fix is in its
 * implementation. One whose per-op cost rises with thread count is contended or
 * serialised — the fix is in its concurrency design. Reading only the 1-thread
 * column has, in this codebase, sent an investigation after the wrong one.
 *
 * <p>usage: {@code PrimCostProbe [threads] [millisPerPoint]}
 *
 * <p><strong>Caveat — the {@code ConcurrentHashMap.get} row is confounded and
 * must not be read as a CHM cost.</strong> Its key is boxed on every call
 * ({@code Integer.valueOf}) and its {@code hashCode} re-enters Java from the
 * native map, so the row measures autoboxing and a native-to-Java transition as
 * much as it measures the map. It is kept because its <em>scaling</em> column is
 * still informative. The allocation and lock rows are clean.
 */
public final class PrimCostProbe {

	private static volatile Object sink;

	public static void main(String[] args) throws Exception {
		final int threads = args.length > 0 ? Integer.parseInt( args[0] ) : 4;
		final long millis = args.length > 1 ? Long.parseLong( args[1] ) : 1500L;

		final ConcurrentHashMap<Integer, String> chm = new ConcurrentHashMap<>();
		for ( int i = 0; i < 256; i++ ) {
			chm.put( i, "v" + i );
		}
		final Map<String, String> hm = new HashMap<>();
		for ( int i = 0; i < 256; i++ ) {
			hm.put( "k" + i, "v" + i );
		}
		final AtomicLong counter = new AtomicLong();
		final ReentrantLock lock = new ReentrantLock();
		final ReentrantReadWriteLock rrwl = new ReentrantReadWriteLock();
		final Object monitor = new Object();

		row( "new Object()", threads, millis, i -> {
			sink = new Object();
		} );
		row( "new long[16]", threads, millis, i -> {
			sink = new long[16];
		} );
		row( "ThreadLocalRandom-free System.nanoTime()", threads, millis, i -> {
			sink = System.nanoTime();
		} );
		row( "Thread.currentThread()", threads, millis, i -> {
			sink = Thread.currentThread();
		} );
		row( "synchronized(obj){}", threads, millis, i -> {
			synchronized ( monitor ) {
				sink = monitor;
			}
		} );
		row( "ReentrantLock lock/unlock", threads, millis, i -> {
			lock.lock();
			try {
				sink = lock;
			}
			finally {
				lock.unlock();
			}
		} );
		row( "RRWL readLock lock/unlock", threads, millis, i -> {
			rrwl.readLock().lock();
			try {
				sink = rrwl;
			}
			finally {
				rrwl.readLock().unlock();
			}
		} );
		row( "AtomicLong.incrementAndGet", threads, millis, i -> {
			sink = counter.incrementAndGet();
		} );
		row( "HashMap.get(String)", threads, millis, i -> {
			sink = hm.get( "k" + ( i & 0xFF ) );
		} );
		// See the class caveat: boxed key + native->Java hashCode re-entry.
		row( "ConcurrentHashMap.get(Integer) [CONFOUNDED]", threads, millis, i -> {
			sink = chm.get( i & 0xFF );
		} );
	}

	private interface Op {
		void run(int i);
	}

	private static void row(String name, int threads, long millis, Op op) throws Exception {
		final double one = perOpNanos( 1, millis, op );
		final double many = perOpNanos( threads, millis, op );
		final double scaling = one <= 0 ? -1.0 : many / one;
		System.out.printf(
				"@@PRIMCOST %-44s 1thread_ns=%9.2f %dthread_ns=%9.2f per_op_ratio=%5.2fx%n",
				name, one, threads, many, scaling );
	}

	/** @return nanoseconds per operation, aggregated across {@code threads}. */
	private static double perOpNanos(int threads, long millis, Op op) throws Exception {
		warm( op );
		final CountDownLatch start = new CountDownLatch( 1 );
		final long[] counts = new long[threads];
		final Thread[] workers = new Thread[threads];
		final long[] deadline = new long[1];

		for ( int t = 0; t < threads; t++ ) {
			final int idx = t;
			workers[t] = new Thread( () -> {
				try {
					start.await();
				}
				catch (InterruptedException e) {
					Thread.currentThread().interrupt();
					return;
				}
				long n = 0;
				int i = idx;
				while ( System.nanoTime() < deadline[0] ) {
					for ( int k = 0; k < 512; k++ ) {
						op.run( i++ );
					}
					n += 512;
				}
				counts[idx] = n;
			} );
			workers[t].start();
		}

		deadline[0] = System.nanoTime() + millis * 1_000_000L;
		start.countDown();
		final long t0 = System.nanoTime();
		long total = 0;
		for ( int t = 0; t < threads; t++ ) {
			workers[t].join();
			total += counts[t];
		}
		final long wall = System.nanoTime() - t0;
		// Aggregate per-op cost: wall time divided by the total ops all threads
		// completed. A perfectly scaling primitive holds this constant as
		// threads rise; a serialised one's rises linearly.
		return total == 0 ? -1.0 : wall / (double) total;
	}

	private static void warm(Op op) {
		for ( int i = 0; i < 20_000; i++ ) {
			op.run( i );
		}
	}
}
