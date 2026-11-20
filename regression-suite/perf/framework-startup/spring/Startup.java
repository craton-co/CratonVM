package probe;

import org.springframework.boot.Banner;
import org.springframework.boot.WebApplicationType;
import org.springframework.boot.builder.SpringApplicationBuilder;
import org.springframework.context.annotation.Bean;
import org.springframework.context.annotation.Configuration;

public class Startup {
    @Configuration(proxyBeanMethods = false)
    static class Config {
        @Bean String greeting() { return "configured"; }
    }

    public static void main(String[] args) {
        var context = new SpringApplicationBuilder(Config.class)
                .web(WebApplicationType.NONE).bannerMode(Banner.Mode.OFF)
                .registerShutdownHook(false).run(args);
        if (!context.isRunning() || !"configured".equals(context.getBean("greeting"))
                || context.getBean("greeting") != context.getBean("greeting")
                || !"configured".equals(context.getEnvironment().getProperty("probe.value"))) {
            throw new AssertionError("Context, singleton or configuration is incomplete");
        }
        System.out.println("CHECK spring-boot configured singleton running");
        System.out.println("READY spring-boot");
        context.close();
        if (context.isActive()) throw new AssertionError("Context did not close");
        System.out.println("CHECK spring-boot closed");
    }
}
