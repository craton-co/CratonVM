import org.springframework.context.annotation.*;
import java.lang.reflect.Field;

public class InheritedBeanProbe {
    public static class Thing { public String toString() { return "Thing"; } }

    @Configuration
    public static class BaseCfg {
        @Bean public Thing thing() { return new Thing(); }
    }

    @Configuration
    public static class SubCfg extends BaseCfg { }

    @Configuration
    public static class OwnCfg { @Bean public Thing thing() { return new Thing(); } }

    static void dump(Class<?> c) {
        System.out.println("class " + c.getName() + " super=" + c.getSuperclass().getName());
        for (Field f : c.getDeclaredFields()) System.out.println("    field " + f.getName() + " : " + f.getType().getName());
    }

    public static void main(String[] a) throws Exception {
        
        try {
            java.lang.reflect.Constructor<?> ctor = Class.forName("org.springframework.context.annotation.ConfigurationClassEnhancer").getDeclaredConstructor();
            ctor.setAccessible(true);
            Object enh = ctor.newInstance();
            java.lang.reflect.Method m = enh.getClass().getDeclaredMethod("enhance", Class.class, ClassLoader.class);
            m.setAccessible(true);
            System.out.println("--- OwnCfg (bean method declared on itself) ---");
            dump((Class<?>) m.invoke(enh, OwnCfg.class, InheritedBeanProbe.class.getClassLoader()));
            System.out.println("--- SubCfg (bean method inherited) ---");
            dump((Class<?>) m.invoke(enh, SubCfg.class, InheritedBeanProbe.class.getClassLoader()));
        } catch (Throwable t) { t.printStackTrace(); }
        System.out.println("--- via context ---");
        try (AnnotationConfigApplicationContext ctx = new AnnotationConfigApplicationContext(SubCfg.class)) {
            System.out.println("thing = " + ctx.getBean(Thing.class));
        } catch (Throwable t) { System.out.println("context FAILED: " + t); }
    }
}
