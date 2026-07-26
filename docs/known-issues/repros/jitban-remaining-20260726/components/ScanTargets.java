package jitprobe.components;

import org.springframework.context.annotation.Bean;
import org.springframework.context.annotation.Configuration;
import org.springframework.stereotype.Component;
import org.springframework.stereotype.Repository;
import org.springframework.stereotype.Service;

public class ScanTargets {

    @Component
    public static class ComponentOne {}

    @Component
    public static class ComponentTwo {}

    @Service
    public static class ServiceOne {}

    @Repository
    public static class RepositoryOne {}

    @Configuration
    public static class ConfigOne {
        @Bean
        public String someBean() {
            return "value";
        }
    }
}
