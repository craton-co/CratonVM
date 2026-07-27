import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.context.annotation.AnnotationConfigApplicationContext;
import org.springframework.context.annotation.Bean;
import org.springframework.context.annotation.Configuration;

public class NullBeanProbe {

    public static class Thing {
        public String toString() { return "Thing"; }
    }

    public static class Holder {
        Thing injected = new Thing();
        @Autowired(required = false)
        public void setThing(Thing t) { this.injected = t; }
    }

    @Configuration
    public static class Cfg {
        @Bean
        public Thing nullThing() { return null; }

        @Bean
        public Holder holder() { return new Holder(); }
    }

    static String describe(Object o) {
        if (o == null) return "<real null>";
        return o.getClass().getName() + " toString=" + o;
    }

    public static void main(String[] args) {
        AnnotationConfigApplicationContext ctx = new AnnotationConfigApplicationContext(Cfg.class);

        Object viaName = ctx.getBean("nullThing");
        System.out.println("getBean(\"nullThing\")            = " + describe(viaName));

        Object viaType;
        try { viaType = ctx.getBean(Thing.class); }
        catch (Throwable t) { viaType = null; System.out.println("getBean(Thing.class) threw " + t.getClass().getName()); }
        System.out.println("getBean(Thing.class)             = " + describe(viaType));

        Holder h = ctx.getBean(Holder.class);
        System.out.println("@Autowired(required=false) field = " + describe(h.injected));

        // Is NullBean's own class even loadable / is instanceof sane?
        try {
            Class<?> nb = Class.forName("org.springframework.beans.factory.support.NullBean");
            System.out.println("NullBean class                   = " + nb + " loader=" + nb.getClassLoader());
            if (viaName != null) {
                System.out.println("viaName instanceof NullBean      = " + nb.isInstance(viaName));
                System.out.println("viaName.getClass()==NullBean     = " + (viaName.getClass() == nb));
                System.out.println("viaName.equals(null)             = " + viaName.equals(null));
            }
        } catch (Throwable t) {
            System.out.println("NullBean lookup failed: " + t);
        }
        ctx.close();
    }
}
