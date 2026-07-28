package probe;

import org.springframework.beans.factory.FactoryBean;
import org.springframework.beans.factory.annotation.Lookup;
import org.springframework.context.annotation.AnnotationConfigApplicationContext;
import org.springframework.context.annotation.Bean;
import org.springframework.context.annotation.Configuration;
import org.springframework.context.annotation.Scope;

/**
 * Exercises the three generated-subclass paths whose defining loader has to
 * follow the SUPERCLASS's loader, not the application loader:
 *
 *  - `@Configuration` CGLIB enhancement (package-private `@Bean` methods that
 *    call each other on `this` must route through the singleton cache);
 *  - the `@Lookup` method-override subclass (the overridden method here is
 *    package-private, which is legal for `@Lookup`);
 *  - the concrete-`FactoryBean` wrapper subclass.
 *
 * `run()` returns a `key=bool;key=bool;...` string so the driver can report
 * each check without the caller needing the fork loader's classes.
 */
public class Driver {

    public static class Engine {
        final int id;

        Engine(int id) {
            this.id = id;
        }
    }

    public static class Car {
        final Engine engine;

        Car(Engine e) {
            this.engine = e;
        }
    }

    public static class Widget {
    }

    public static class Gadget {
    }

    static int engineCtor = 0;

    /** Abstract bean class whose `@Lookup` method is PACKAGE-PRIVATE. */
    public abstract static class WidgetUser {
        @Lookup("widget")
        abstract Widget obtainWidget();

        public Object callLookup() {
            return obtainWidget();
        }
    }

    public static class GadgetFactory implements FactoryBean<Gadget> {
        @Override
        public Gadget getObject() {
            return new Gadget();
        }

        @Override
        public Class<?> getObjectType() {
            return Gadget.class;
        }

        @Override
        public boolean isSingleton() {
            return false;
        }
    }

    /** Package-private `@Bean` methods, exactly as in the original repro. */
    @Configuration
    static class Cfg {
        @Bean
        Engine engine() {
            return new Engine(engineCtor++);
        }

        @Bean
        Car car1() {
            return new Car(engine());
        }

        @Bean
        Car car2() {
            return new Car(engine());
        }

        @Bean
        @Scope("prototype")
        Widget widget() {
            return new Widget();
        }

        @Bean
        WidgetUser widgetUser() {
            // Abstract: Spring must generate the method-override subclass.
            return null;
        }

        @Bean
        GadgetFactory gadgetFactory() {
            return new GadgetFactory();
        }
    }

    public static String run() {
        StringBuilder sb = new StringBuilder();
        engineCtor = 0;
        AnnotationConfigApplicationContext ctx = new AnnotationConfigApplicationContext();
        ctx.setClassLoader(Driver.class.getClassLoader());
        ctx.register(Cfg.class);
        // `@Lookup` needs a concrete bean DEFINITION for the abstract class.
        ctx.registerBean("widgetUser", WidgetUser.class);
        ctx.refresh();

        Engine e = ctx.getBean(Engine.class);
        Car c1 = ctx.getBean("car1", Car.class);
        Car c2 = ctx.getBean("car2", Car.class);
        sb.append("engineBuiltOnce=").append(engineCtor == 1).append(';');
        sb.append("car1SharesEngine=").append(c1.engine == e).append(';');
        sb.append("car2SharesEngine=").append(c2.engine == e).append(';');
        sb.append("cfgEnhanced=")
                .append(ctx.getBean(Cfg.class).getClass() != Cfg.class)
                .append(';');

        // `@Lookup`: the generated subclass overrides a PACKAGE-PRIVATE method.
        System.out.println("[DIAG] cfgClass=" + ctx.getBean(Cfg.class).getClass().getName()
                + " loader=" + ctx.getBean(Cfg.class).getClass().getClassLoader());
        try {
            WidgetUser u = ctx.getBean("widgetUser", WidgetUser.class);
            System.out.println("[DIAG] widgetUserClass=" + u.getClass().getName()
                    + " loader=" + u.getClass().getClassLoader()
                    + " superLoader=" + WidgetUser.class.getClassLoader());
            sb.append("lookupSubclassed=").append(u.getClass() != WidgetUser.class).append(';');
            // The generated subclass must be defined by the SAME loader as its
            // superclass (HotSpot/cglib do this); otherwise it is in a
            // different runtime package and its package-private override of
            // `obtainWidget()` is not an override at all.
            sb.append("lookupSubclassSameLoader=")
                    .append(u.getClass().getClassLoader() == WidgetUser.class.getClassLoader())
                    .append(';');
            Object w1 = u.callLookup();
            Object w2 = u.callLookup();
            sb.append("lookupReturnsBean=").append(w1 instanceof Widget).append(';');
            sb.append("lookupIsPrototype=").append(w1 != w2).append(';');
        } catch (Throwable t) {
            sb.append("lookupThrew_").append(t.getClass().getSimpleName()).append("=false;");
        }

        // Concrete FactoryBean: CratonVM wraps it in a generated subclass.
        try {
            Object g = ctx.getBean("gadgetFactory");
            sb.append("factoryBeanProduct=").append(g instanceof Gadget).append(';');
            Object raw = ctx.getBean("&gadgetFactory");
            System.out.println("[DIAG] factoryBeanRawClass=" + raw.getClass().getName()
                    + " loader=" + raw.getClass().getClassLoader());
            sb.append("factoryBeanRaw=").append(raw instanceof GadgetFactory).append(';');
        } catch (Throwable t) {
            sb.append("factoryBeanThrew_").append(t.getClass().getSimpleName()).append("=false;");
        }

        ctx.close();
        return sb.toString();
    }
}
