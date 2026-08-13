import java.util.HashMap;
import java.util.Map;

/**
 * Hashing an object while its own monitor is held must answer the same value as
 * hashing it before or after. Anything else silently breaks every `HashMap`
 * keyed by an object that does not override `hashCode`, because the entry is
 * filed under one hash and looked up under another.
 *
 * Spring Boot's `TomcatWebServer` does exactly this: it parks the connectors in
 * a `Map<Service, Connector[]>` from inside `LifecycleBase.start()`, which is
 * `synchronized` on the very `StandardService` it uses as the key.
 */
public final class IdentityHashWhileLockedProbe {

	public static void main(String[] args) {
		Object o = new Object();
		int outsideBefore = System.identityHashCode(o);
		int inside;
		synchronized (o) {
			inside = System.identityHashCode(o);
		}
		int outsideAfter = System.identityHashCode(o);
		System.out.println("hashed-first : before=" + outsideBefore + " inside=" + inside + " after=" + outsideAfter
				+ " stable=" + (outsideBefore == inside && inside == outsideAfter));

		Object p = new Object();
		int insideFirst;
		synchronized (p) {
			insideFirst = System.identityHashCode(p);
		}
		int afterUnlock = System.identityHashCode(p);
		System.out.println("locked-first : inside=" + insideFirst + " afterUnlock=" + afterUnlock + " stable="
				+ (insideFirst == afterUnlock));

		Object key = new Object();
		Map<Object, String> map = new HashMap<>();
		synchronized (key) {
			map.put(key, "value");
		}
		map.put(key, "again");
		System.out.println("map put under the key's own lock: get=" + map.get(key) + " size=" + map.size()
				+ " (1 is correct)");
	}

}
