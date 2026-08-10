import java.util.HashMap;
import java.util.Map;
import java.util.concurrent.CountDownLatch;

/**
 * The identity hash lives in the mark word until the object's monitor inflates,
 * at which point it is displaced into the monitor. The question this asks is
 * what an object that inflates BEFORE it is ever hashed answers — and whether
 * that answer survives the monitor going away again.
 *
 * Tomcat's `StandardService` takes exactly that route: `LifecycleBase.start()`
 * is `synchronized`, and the object is only hashed afterwards, when Spring
 * Boot's `TomcatWebServer` parks its connectors in a `Map<Service, ...>`.
 */
public final class IdentityHashAfterInflationProbe {

	public static void main(String[] args) throws Exception {
		hashWhileInflated();
		hashAfterInflationReleased();
		mapPutWhileInflated();
	}

	/** Hash it while another thread owns the monitor, then again after. */
	private static void hashWhileInflated() throws Exception {
		Object o = new Object();
		Holder holder = Holder.inflate(o);
		int whileHeld = System.identityHashCode(o);
		holder.release();
		int afterRelease = System.identityHashCode(o);
		System.out.println("hashWhileInflated: whileHeld=" + whileHeld + " afterRelease=" + afterRelease + " stable="
				+ (whileHeld == afterRelease));
	}

	/** Inflate first, hash only once the monitor is idle again. */
	private static void hashAfterInflationReleased() throws Exception {
		Object o = new Object();
		Holder holder = Holder.inflate(o);
		holder.release();
		int first = System.identityHashCode(o);
		churn();
		int second = System.identityHashCode(o);
		System.out.println("hashAfterInflationReleased: first=" + first + " second=" + second + " stable="
				+ (first == second));
	}

	/** The Tomcat shape: put into a map while inflated, look it up later. */
	private static void mapPutWhileInflated() throws Exception {
		Object key = new Object();
		Map<Object, String> map = new HashMap<>();
		Holder holder = Holder.inflate(key);
		map.put(key, "value");
		holder.release();
		churn();
		System.out.println("mapPutWhileInflated: containsKey=" + map.containsKey(key) + " get=" + map.get(key));
		map.put(key, "again");
		System.out.println("mapPutWhileInflated: size after re-put with the same key=" + map.size() + " (1 is correct)");
	}

	private static void churn() {
		long sink = 0;
		for (int i = 0; i < 512; i++) {
			byte[] block = new byte[64 * 1024];
			block[0] = (byte) i;
			sink += block[0];
		}
		System.gc();
		if (sink == Long.MIN_VALUE) {
			System.out.println("unreachable " + sink);
		}
	}

	/** Forces `target`'s monitor to inflate by contending for it from two threads. */
	private static final class Holder {

		private final Object target;

		private final CountDownLatch releaseLatch = new CountDownLatch(1);

		private Thread owner;

		private Thread contender;

		private Holder(Object target) {
			this.target = target;
		}

		static Holder inflate(Object target) throws InterruptedException {
			Holder holder = new Holder(target);
			CountDownLatch holding = new CountDownLatch(1);
			holder.owner = new Thread(() -> {
				synchronized (target) {
					holding.countDown();
					await(holder.releaseLatch);
				}
			});
			holder.owner.start();
			holding.await();
			holder.contender = new Thread(() -> {
				synchronized (target) {
					// only entered once the owner lets go — this is the contention
					// that forces inflation
				}
			});
			holder.contender.start();
			Thread.sleep(100);
			return holder;
		}

		void release() throws InterruptedException {
			this.releaseLatch.countDown();
			this.owner.join();
			this.contender.join();
			synchronized (this.target) {
				// take and drop it once more from this thread
			}
		}

		private static void await(CountDownLatch latch) {
			try {
				latch.await();
			}
			catch (InterruptedException ex) {
				Thread.currentThread().interrupt();
			}
		}

	}

}
