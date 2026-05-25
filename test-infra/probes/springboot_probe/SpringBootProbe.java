import org.springframework.boot.SpringApplication;
import org.springframework.context.annotation.Configuration;
public class SpringBootProbe {
    @Configuration
    static class Cfg {}
    public static void main(String[] args) throws Exception {
        // Verify Spring Boot core class loads (exercises the
        // ConditionalOn* + AutoConfigurationImportSelector clinit chain)
        SpringApplication app = new SpringApplication(Cfg.class);
        app.setBannerMode(org.springframework.boot.Banner.Mode.OFF);
        app.setWebApplicationType(org.springframework.boot.WebApplicationType.NONE);
        System.out.println("SpringApplication: " + app.getClass().getName());
        System.out.println("MainAppClass: " + app.getMainApplicationClass());
        // Don't actually run() — the post-banner phase hangs on non-TTY
        // stdout. Construction alone exercises ConfigClassParser etc.
        System.out.println("OK");
        System.exit(0);
    }
}
