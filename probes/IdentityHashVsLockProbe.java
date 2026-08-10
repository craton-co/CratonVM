import java.util.HashMap;
import java.util.Map;
import java.util.concurrent.CountDownLatch;

/**
 * An object's identity hash must survive everything that touches its header.
 * Locking is the classic collision: on HotSpot the mark word holds either the
 * hash or the lock state, and displacing one must not lose the other.
 *
 * A lost hash is not a cosmetic defect — every `HashMap` keyed by an object
 * that does not override `hashCode` stops finding its own entries, which is how
 * Spring Boot's `TomcatWebServer` lost the connectors it parked in
 * `Map<Service, Connector[]>` across a context start.
 */
public final class IdentityHashVsLockProbe {

	public static void main(String[] args) throws Exception {
		hashThenLock();
		lockThenHash();
		hashThenContend();
		mapAcrossLock();
	}

	private static void hashThenLock() {
		Object o = new Object();
		int before = System.identityHashCode(o);
		synchronized (o) {
			// uncontended enter/exit
		}
		int after = System.identityHashCode(o);
		System.out.println("hashThenLock: before=" + before + " after=" + after + " stable=" + (before == after));
	}

	private static void lockThenHash() {
		Object o = new Object();
		synchronized (o) {
			// uncontended enter/exit
		}
		int first = System.identityHashCode(o);
		synchronized (o) {
			// again
		}
		int second = System.identityHashCode(o);
		System.out.println("lockThenHash: first=" + first + " second=" + second + " stable=" + (first == second));
	}

	private static void hashThenContend() throws Exception {
		Object o = new Object();
		int before = System.identityHashCode(o);
		CountDownLatch holding = new CountDownLatch(1);
		CountDownLatch release = new CountDownLatch(1);
		Thread other = new Thread(() -> {
			synchronized (o) {
				holding.countDown();
				try {
					release.await();
				}
				catch (InterruptedException ex) {
					Thread.currentThread().interrupt();
				}
			}
		});
		other.start();
		holding.await();
		release.countDown();
		synchronized (o) {
			// contend for the same monitor from this thread
		}
		other.join();
		int after = System.identityHashCode(o);
		System.out.println("hashThenContend: before=" + before + " after=" + after + " stable=" + (before == after));
	}

	private static void mapAcrossLock() {
		Object key = new Object();
		Map<Object, String> map = new HashMap<>();
		map.put(key, "value");
		synchronized (key) {
			// the same shape Tomcat's Service goes through between put and get
		}
		System.out.println("mapAcrossLock: containsKey=" + map.containsKey(key) + " get=" + map.get(key) + " size="
				+ map.size());
	}

}
