import org.springframework.beans.factory.support.DefaultListableBeanFactory;
import org.springframework.beans.factory.support.RootBeanDefinition;

// SPB.9b (Session 114) partial repro: this exercises the
// org/springframework/beans/factory/support/ sub-package (real
// DefaultListableBeanFactory.registerBeanDefinition + RootBeanDefinition
// construction) -- the ban's own note describes ~50+ beans being
// registered per scanned Spring Boot 3.2 reactive app, each
// RootBeanDefinition.<init> copying ~15 fields via putfield (the exact
// W2-CHM allocate-then-putfield archetype). This does NOT cover the
// other two sub-bans in the same SPB.9b group
// (org/springframework/boot/loader/ -- JarLauncher/executable-jar
// classloading; org/springframework/web/reactive/ +
// org/springframework/boot/web/reactive/ -- WebFlux server boot), which
// need the full insurance-backend app scaffold (confirmed absent from
// this host) to test faithfully.
public class BeanFactorySupportProbe {

    public static class BeanA {} public static class BeanB {} public static class BeanC {}
    public static class BeanD {} public static class BeanE {} public static class BeanF {}
    public static class BeanG {} public static class BeanH {} public static class BeanI {}
    public static class BeanJ {}

    private static final Class<?>[] TYPES = {
        BeanA.class, BeanB.class, BeanC.class, BeanD.class, BeanE.class,
        BeanF.class, BeanG.class, BeanH.class, BeanI.class, BeanJ.class,
    };

    public static void main(String[] args) throws Exception {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 500;
        int beansPerIteration = 60; // matches the "~50+ beans" scale the ban describes
        for (int i = 0; i < iterations; i++) {
            DefaultListableBeanFactory factory = new DefaultListableBeanFactory();
            for (int b = 0; b < beansPerIteration; b++) {
                Class<?> type = TYPES[b % TYPES.length];
                RootBeanDefinition def = new RootBeanDefinition(type);
                def.setScope("singleton");
                def.setLazyInit(b % 2 == 0);
                def.setPrimary(b % 5 == 0);
                def.setAutowireCandidate(true);
                factory.registerBeanDefinition("bean-" + i + "-" + b, def);
            }
            if (factory.getBeanDefinitionCount() != beansPerIteration) {
                System.out.println("RESULT: FAIL at iteration " + i
                        + " -- expected " + beansPerIteration + " registered beans, got "
                        + factory.getBeanDefinitionCount());
                System.exit(1);
            }
            // Exercise real reads back out of the registry, matching the
            // ban's own description of fields being read after the
            // allocate-then-putfield construction.
            for (String name : factory.getBeanDefinitionNames()) {
                RootBeanDefinition def = (RootBeanDefinition) factory.getBeanDefinition(name);
                if (def.getBeanClass() == null) {
                    System.out.println("RESULT: FAIL at iteration " + i
                            + " -- getBeanClass() returned null for " + name);
                    System.exit(1);
                }
            }
        }
        System.out.println("RESULT: OK -- " + iterations + " x " + beansPerIteration
                + " RootBeanDefinition registrations via real DefaultListableBeanFactory, all consistent");
    }
}
