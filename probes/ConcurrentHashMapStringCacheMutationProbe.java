import java.util.concurrent.ConcurrentHashMap;

/**
 * Ensures the CHM String-node memo never returns a removed or replaced node.
 * Each read is deliberately preceded by a hot-cache lookup for the same key.
 */
public final class ConcurrentHashMapStringCacheMutationProbe {
    public static void main(String[] args) {
        ConcurrentHashMap<String, String> map = new ConcurrentHashMap<>();
        map.put("key", "one");
        for (int i = 0; i < 10_000; i++) {
            if (!"one".equals(map.get("key"))) {
                throw new AssertionError("warm lookup");
            }
        }
        map.put("key", "two");
        if (!"two".equals(map.get("key"))) {
            throw new AssertionError("replacement leaked stale node");
        }
        if (!"two".equals(map.remove("key"))) {
            throw new AssertionError("remove result");
        }
        if (map.get("key") != null) {
            throw new AssertionError("removed mapping leaked from cache");
        }
        map.put("key", "three");
        if (!"three".equals(map.get("key"))) {
            throw new AssertionError("reinserted mapping");
        }
        System.out.println("CHM_STRING_CACHE_MUTATION_OK");
    }
}
