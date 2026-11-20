package probe;

import jakarta.enterprise.context.ApplicationScoped;
import jakarta.inject.Inject;
import io.quarkus.runtime.QuarkusApplication;
import io.quarkus.runtime.annotations.QuarkusMain;
import org.eclipse.microprofile.config.inject.ConfigProperty;

@QuarkusMain
public class Main implements QuarkusApplication {
    @ApplicationScoped
    public static class Greeting {
        @ConfigProperty(name = "probe.value") String value;
        public String value() { return value; }
    }

    @Inject Greeting greeting;
    @Inject Greeting sameGreeting;

    public int run(String... args) {
        if (!"configured".equals(greeting.value()) || greeting != sameGreeting) {
            throw new AssertionError("CDI or configuration is incomplete");
        }
        System.out.println("CHECK quarkus configured singleton running");
        System.out.println("READY quarkus");
        return 0;
    }
}
