import org.springframework.context.annotation.AnnotationConfigApplicationContext;

/**
 * Builds the exact context `EnableCachingTests` builds, and prints the FULL
 * cause chain — which the suite runner's FAILCAUSE line truncates to the
 * outermost `BeanCreationException` message.
 */
public class CacheCtxProbe {
    public static void main(String[] args) throws Exception {
        Class<?> cfg = Class.forName(
                "org.springframework.cache.config.EnableCachingTests$EnableCachingConfig");
        int iters = Integer.getInteger("probe.iters", 40);
        for (int i = 0; i < iters; i++) {
            try (AnnotationConfigApplicationContext ctx =
                         new AnnotationConfigApplicationContext(cfg)) {
                if (ctx.getBeanDefinitionCount() == 0) {
                    System.out.println("ITER " + i + " EMPTY");
                }
            } catch (Throwable t) {
                System.out.println("ITER " + i + " THREW");
                for (Throwable c = t; c != null; c = c.getCause()) {
                    System.out.println("  CAUSE " + c.getClass().getName() + ": " + c.getMessage());
                    StackTraceElement[] st = c.getStackTrace();
                    for (int k = 0; k < Math.min(6, st.length); k++) {
                        System.out.println("      at " + st[k]);
                    }
                    if (c.getCause() == c) break;
                }
                System.out.println("PROBE-RESULT failed_at_iter=" + i);
                return;
            }
        }
        System.out.println("PROBE-RESULT all " + iters + " contexts built OK");
    }
}
