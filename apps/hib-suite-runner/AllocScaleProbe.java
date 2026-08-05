import java.util.concurrent.CountDownLatch;

/**
 * Allocation throughput against thread count, for telling a merely-slow
 * allocator apart from one that does not scale.
 *
 * <p>This exists because the two present very differently in a profile and
 * identically in a wall-clock number. A serialised allocator keeps every core
 * busy — spinning on the bump mutex reads as CPU-busy, not as blocked — so the
 * usual "4.3 of 5 threads are running" check cannot see it. Aggregate
 * throughput against thread count can: a scaling allocator's aggregate rate
 * rises with threads, a serialised one's is flat.
 *
 * <p>Two shapes are measured because they can differ. {@code new Object()} is a
 * fixed-size header-only allocation; {@code new long[16]} additionally zeroes a
 * payload, so if the allocator holds its lock across the zeroing the hold time
 * scales with size and arrays serialise while objects do not. That is exactly
 * the asymmetry this probe was written to catch.
 *
 * <p>usage: {@code AllocScaleProbe [maxThreads] [millisPerPoint]} — runs 1..N
 * threads for each shape and prints one {@code @@ALLOCSCALE} line per point
 * plus a {@code @@ALLOCSCALE-SUMMARY} with the aggregate scaling factor from
 * one thread to {@code maxThreads}. A scaling factor near 1.0 is a serialised
 * allocator; near {@code maxThreads} is a perfectly scaling one.
 */
public final class AllocScaleProbe {

	/**
	 * Where each thread parks its allocations so they cannot be optimised away.
	 *
	 * This has to be a real, escaping store per allocation. An earlier version
	 * assigned each new object to a local and published only the last one after
	 * the loop — which lets escape analysis prove every other allocation
	 * non-escaping and scalar-replace it. The {@code Object} row then measured
	 * an empty loop and reported ~1.0x "scaling" for an allocator that was
	 * never called, i.e. exactly the reading this probe exists to distinguish
	 * from a serialised allocator.
	 *
	 * A per-thread ring keeps the store escaping without retaining memory
	 * without bound, and — being per thread — adds no cross-core traffic of its
	 * own. A single shared {@code volatile} sink would serialise the
	 * measurement on the sink instead of the allocator, which is the same
	 * mistake in the opposite direction.
	 */
	private static final int RING = 64;

	/** Published once per run so the rings cannot be dead-code-eliminated. */
	private static volatile Object keepAlive;

	public static void main(String[] args) throws Exception {
		final int maxThreads = args.length > 0 ? Integer.parseInt( args[0] ) : 4;
		final long millisPerPoint = args.length > 1 ? Long.parseLong( args[1] ) : 2000L;

		for ( String shape : new String[] { "Object", "long[16]" } ) {
			long oneThreadRate = -1;
			long maxThreadRate = -1;
			for ( int t = 1; t <= maxThreads; t++ ) {
				// One untimed round per point: the first allocations on a fresh
				// thread take its TLAB refill path, which is not what is being
				// measured.
				measure( shape, t, 300L );
				final long total = measure( shape, t, millisPerPoint );
				final long rate = total * 1000L / millisPerPoint;
				if ( t == 1 ) {
					oneThreadRate = rate;
				}
				if ( t == maxThreads ) {
					maxThreadRate = rate;
				}
				System.out.printf( "@@ALLOCSCALE shape=%s threads=%d allocs=%d allocs_per_s=%d%n",
						shape, t, total, rate );
			}
			final double scaling = oneThreadRate <= 0
					? -1.0
					: maxThreadRate / (double) oneThreadRate;
			System.out.printf(
					"@@ALLOCSCALE-SUMMARY shape=%s threads=1..%d aggregate_scaling=%.2fx"
							+ " (1.00x means fully serialised, %.2fx means perfect)%n",
					shape, maxThreads, scaling, (double) maxThreads );
		}
	}

	/** @return total allocations performed across all threads in the window. */
	private static long measure(String shape, int threads, long millis) throws Exception {
		final CountDownLatch start = new CountDownLatch( 1 );
		final long[] counts = new long[threads];
		final Thread[] workers = new Thread[threads];
		final long[] deadline = new long[1];

		for ( int i = 0; i < threads; i++ ) {
			final int idx = i;
			workers[i] = new Thread( () -> {
				try {
					start.await();
				}
				catch (InterruptedException e) {
					Thread.currentThread().interrupt();
					return;
				}
				long n = 0;
				final Object[] ring = new Object[RING];
				final boolean objects = "Object".equals( shape );
				// Check the clock once per batch, not once per allocation: a
				// nanoTime call per iteration would dominate the very cost
				// being measured.
				while ( System.nanoTime() < deadline[0] ) {
					for ( int k = 0; k < 1024; k++ ) {
						ring[k & ( RING - 1 )] = objects ? new Object() : new long[16];
					}
					n += 1024;
				}
				keepAlive = ring;
				counts[idx] = n;
			} );
			workers[i].start();
		}

		deadline[0] = System.nanoTime() + millis * 1_000_000L;
		start.countDown();
		long total = 0;
		for ( int i = 0; i < threads; i++ ) {
			workers[i].join();
			total += counts[i];
		}
		return total;
	}
}
