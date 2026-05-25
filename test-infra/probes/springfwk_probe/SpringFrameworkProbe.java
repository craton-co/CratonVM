import org.springframework.context.annotation.AnnotationConfigApplicationContext;
import org.springframework.context.annotation.Configuration;
import org.springframework.context.annotation.Bean;
import org.springframework.core.SpringVersion;
public class SpringFrameworkProbe {
    @Configuration
    static class Cfg {
        @Bean public String greeting() { return "hello from probe"; }
    }
    public static void main(String[] args) {
        try {
            System.out.println("Spring version: " + SpringVersion.getVersion());
            AnnotationConfigApplicationContext ctx = new AnnotationConfigApplicationContext(Cfg.class);
            String g = ctx.getBean(String.class);
            System.out.println("Bean: " + g);
            if (!"hello from probe".equals(g)) { System.out.println("FAIL"); System.exit(1); }
            ctx.close();
            System.out.println("OK");
        } catch (Throwable t) { t.printStackTrace(); System.exit(1); }
        System.exit(0);
    }
}
