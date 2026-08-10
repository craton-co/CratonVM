import java.util.HashMap;
import java.util.IdentityHashMap;
import java.util.Map;

/**
 * `Object.hashCode()`/`System.identityHashCode` must answer the same value for
 * the lifetime of an object, whatever the collector does to its address —
 * otherwise every `HashMap` keyed by an object that does not override
 * `hashCode` silently loses its entries at the next collection.
 *
 * Spring Boot's `TomcatWebServer` is exactly such a map: it parks the service's
 * connectors in a `Map<Service, Connector[]>` while the context starts, then
 * looks them up again by the same `StandardService` instance. A miss there
 * leaves the service with no connectors, so `Tomcat.getConnector()` fabricates
 * a fresh port-8080 one and the server fails to start.
 */
public final class IdentityHashAcrossGcProbe {

	public static void main(String[] args) {
		Object plain = new Object();
		Holder holder = new Holder("held");

		int plainBefore = System.identityHashCode(plain);
		int holderBefore = System.identityHashCode(holder);
		int plainHashBefore = plain.hashCode();

		Map<Object, String> hashMap = new HashMap<>();
		hashMap.put(plain, "plain-value");
		hashMap.put(holder, "holder-value");
		Map<Object, String> identityMap = new IdentityHashMap<>();
		identityMap.put(plain, "plain-value");
		identityMap.put(holder, "holder-value");

		churn();

		int plainAfter = System.identityHashCode(plain);
		int holderAfter = System.identityHashCode(holder);
		int plainHashAfter = plain.hashCode();

		System.out.println("identityHashCode(plain)  before=" + plainBefore + " after=" + plainAfter + " stable="
				+ (plainBefore == plainAfter));
		System.out.println("Object.hashCode(plain)   before=" + plainHashBefore + " after=" + plainHashAfter
				+ " stable=" + (plainHashBefore == plainHashAfter));
		System.out.println("identityHashCode(holder) before=" + holderBefore + " after=" + holderAfter + " stable="
				+ (holderBefore == holderAfter));
		System.out.println("HashMap.get(plain)=" + hashMap.get(plain));
		System.out.println("HashMap.get(holder)=" + hashMap.get(holder));
		System.out.println("HashMap.size=" + hashMap.size());
		System.out.println("IdentityHashMap.get(plain)=" + identityMap.get(plain));
		System.out.println("IdentityHashMap.get(holder)=" + identityMap.get(holder));
	}

	/** Allocate enough short-lived garbage to force several collections. */
	private static void churn() {
		long sink = 0;
		for (int i = 0; i < 4096; i++) {
			byte[] block = new byte[64 * 1024];
			block[0] = (byte) i;
			sink += block[0];
		}
		System.gc();
		if (sink == Long.MIN_VALUE) {
			System.out.println("unreachable " + sink);
		}
	}

	/** A class with no `hashCode` override — identity hashing, like `StandardService`. */
	static final class Holder {

		@SuppressWarnings("unused")
		private final String name;

		Holder(String name) {
			this.name = name;
		}

	}

}
