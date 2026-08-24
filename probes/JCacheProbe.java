import javax.cache.Cache;
import javax.cache.CacheManager;
import javax.cache.Caching;
import javax.cache.configuration.MutableConfiguration;
import javax.cache.spi.CachingProvider;

/**
 * The `@BeforeEach` of `JCacheEhCacheApiTests`, standalone.
 *
 * On CratonVM every one of the 82 methods across `JCacheEhCacheApiTests` and
 * `JCacheEhCacheAnnotationTests` dies with
 * `javax.cache.CacheException: org.ehcache.StateTransitionException`, and the
 * suite runner prints only that summary — the wrapped exception's own message
 * is empty, so the cause chain is the entire diagnosis and the runner discards
 * it. This prints the chain.
 */
public class JCacheProbe {

    static void p(String k, Object v) {
        System.out.println(k + " = " + v);
    }

    public static void main(String[] args) {
        try {
            CachingProvider provider =
                    Caching.getCachingProvider("org.ehcache.jsr107.EhcacheCachingProvider");
            p("P01 provider", provider.getClass().getName());

            CacheManager cm = provider.getCacheManager();
            p("P02 cacheManager", cm.getClass().getName());
            p("P03 manager closed", cm.isClosed());

            cm.createCache("testCache", new MutableConfiguration<>());
            p("P04 createCache testCache", "ok");
            cm.createCache("testCacheNoNull", new MutableConfiguration<>());
            p("P05 createCache testCacheNoNull", "ok");

            Cache<Object, Object> c = cm.getCache("testCache");
            p("P06 getCache non-null", c != null);

            c.put("k", "v");
            p("P07 put/get round-trips", c.get("k"));
            c.remove("k");
            p("P08 removed", c.get("k"));
            cm.close();
            p("P09 closed cleanly", cm.isClosed());
        }
        catch (Throwable t) {
            System.out.println("THREW");
            int depth = 0;
            for (Throwable e = t; e != null && depth < 12; e = e.getCause(), depth++) {
                System.out.println("  [" + depth + "] " + e.getClass().getName()
                        + ": " + e.getMessage());
                StackTraceElement[] st = e.getStackTrace();
                for (int i = 0; i < Math.min(8, st.length); i++) {
                    System.out.println("        at " + st[i]);
                }
                if (e.getCause() == e) {
                    break;
                }
            }
        }
    }
}
