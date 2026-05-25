import org.springframework.boot.Banner;
import org.springframework.boot.WebApplicationType;
import org.springframework.boot.builder.SpringApplicationBuilder;
import org.springframework.context.ConfigurableApplicationContext;
import org.springframework.context.annotation.Bean;
import org.springframework.context.annotation.Configuration;

public class SpringBootProbe {
    @Configuration
    static class Cfg {
        @Bean public String probeMessage() { return "hello from probe"; }
    }
    public static void main(String[] args) throws Exception {
        ConfigurableApplicationContext ctx = new SpringApplicationBuilder(Cfg.class)
            .web(WebApplicationType.NONE)
            .bannerMode(Banner.Mode.OFF)
            .registerShutdownHook(false)
            .run(args);
        String msg = ctx.getBean(String.class);
        System.out.println("Bean: " + msg);
        ctx.close();
        System.out.println("OK");
        System.exit(0);
    }
}
