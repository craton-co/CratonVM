import com.sun.management.ThreadMXBean;
import java.lang.management.ManagementFactory;
import java.util.Locale;

import org.hibernate.boot.MetadataSources;
import org.hibernate.boot.registry.StandardServiceRegistry;
import org.hibernate.boot.registry.StandardServiceRegistryBuilder;
import org.hibernate.cfg.QuerySettings;
import org.hibernate.engine.spi.SessionFactoryImplementor;
import org.hibernate.query.hql.HqlTranslator;

/**
 * `HqlParserMemoryUsageTest`'s measurement window, with BOTH allocation
 * counters read across it instead of one.
 *
 * The test itself prints only the figure `MemoryUsageUtil` chose --
 * `getTotalThreadAllocatedBytes`, the process-wide counter -- and asserts a
 * 256 MiB budget against it. When that counter was over-reporting there was
 * nothing in the test's own output to say so: a 627,662 KB reading and a
 * 248,314 KB reading look equally like measurements. Reading the per-thread
 * counter alongside it turns the question into a comparison, and the two
 * counters are independent enough mechanisms that agreement is evidence.
 *
 * Bootstraps the same seven entities and translates the same HQL as the test
 * (both copied from it), through the plain Hibernate bootstrap API rather than
 * the JUnit extension, so it runs on a bare classpath.
 *
 * Usage:
 *   <vm> -cp <hibernate test classpath>:<this> HqlParseAllocProbe [repeats]
 */
public class HqlParseAllocProbe {
    static final ThreadMXBean TMX = (ThreadMXBean) ManagementFactory.getThreadMXBean();

    /** Copied verbatim from org.hibernate.orm.test.hql.HqlParserMemoryUsageTest. */
    private static final String HQL = """
            SELECT DISTINCT u.id
            FROM AppUser u
            LEFT JOIN u.addresses a
            LEFT JOIN u.orders o
            LEFT JOIN o.orderItems oi
            LEFT JOIN oi.product p
            LEFT JOIN p.discounts d
            WHERE u.id = :userId
            AND (
                CASE
                    WHEN u.name = 'SPECIAL_USER' THEN TRUE
                    ELSE (
                        CASE
                            WHEN a.city = 'New York' THEN TRUE
                            ELSE (
                                p.category.name = 'Electronics'
                                OR d.code LIKE '%DISC%'
                                OR u.id IN (
                                    SELECT u2.id
                                    FROM AppUser u2
                                    JOIN u2.orders o2
                                    JOIN o2.orderItems oi2
                                    JOIN oi2.product p2
                                    WHERE p2.price > (
                                        SELECT AVG(p3.price) FROM Product p3
                                    )
                                )
                            )
                        END
                    )
                END
            )
            """;

    public static void main(String[] args) {
        int repeats = args.length > 0 ? Integer.parseInt(args[0]) : 3;

        StandardServiceRegistry registry = new StandardServiceRegistryBuilder()
                // The test's own @ServiceRegistry setting: an enabled plan cache
                // would make every repeat after the first measure a map lookup.
                .applySetting(QuerySettings.QUERY_PLAN_CACHE_ENABLED, "false")
                .applySetting("hibernate.connection.driver_class", "org.h2.Driver")
                .applySetting("hibernate.connection.url",
                        "jdbc:h2:mem:hqlparseallocprobe;DB_CLOSE_DELAY=-1")
                .applySetting("hibernate.connection.username", "sa")
                .applySetting("hibernate.connection.password", "")
                .applySetting("hibernate.hbm2ddl.auto", "create-drop")
                .applySetting("hibernate.show_sql", "false")
                .build();

        MetadataSources sources = new MetadataSources(registry);
        for (String n : new String[] {
                "Address", "AppUser", "Category", "Discount", "Order", "OrderItem", "Product" }) {
            sources.addAnnotatedClass(entity(n));
        }

        try (SessionFactoryImplementor sf =
                (SessionFactoryImplementor) sources.buildMetadata().buildSessionFactory()) {
            HqlTranslator translator = sf.getQueryEngine().getHqlTranslator();
            // The test's own warm-up line: "Ensure classes and basic stuff is
            // initialized in case this is the first test run".
            translator.translate("from AppUser", Object.class);

            for (int i = 0; i < repeats; i++) {
                long ptBefore = TMX.getCurrentThreadAllocatedBytes();
                long prBefore = TMX.getTotalThreadAllocatedBytes();
                translator.translate(HQL, Long.class);
                long pt = TMX.getCurrentThreadAllocatedBytes() - ptBefore;
                long pr = TMX.getTotalThreadAllocatedBytes() - prBefore;
                System.out.println(String.format(Locale.ROOT,
                        "parse %d: process_wide=%d KB  per_thread=%d KB  ratio=%.2f  budget=%s",
                        i, pr / 1024, pt / 1024,
                        pt == 0 ? Double.NaN : pr / (double) pt,
                        pr < 268_435_456L ? "PASS" : "FAIL"));
            }
        }
        StandardServiceRegistryBuilder.destroy(registry);
        System.out.println("HQLPARSEALLOC_END");
    }

    /**
     * The entities are public static nested classes of the test, which is on
     * the same (test) classpath. Named reflectively so this file does not have
     * to import a test class and can still be compiled against main-only jars.
     */
    static Class<?> entity(String simpleName) {
        try {
            return Class.forName(
                    "org.hibernate.orm.test.hql.HqlParserMemoryUsageTest$" + simpleName);
        }
        catch (ClassNotFoundException e) {
            throw new IllegalStateException(
                    "HqlParserMemoryUsageTest must be on the classpath: " + e, e);
        }
    }
}
