package org.keycloak.quarkus.runtime.configuration;

import java.nio.file.Paths;
import java.io.FileInputStream;
import java.security.KeyStore;
import java.util.ArrayList;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.HashMap;
import java.util.Map;
import java.util.Properties;

import org.keycloak.quarkus.runtime.Environment;
import org.keycloak.quarkus.runtime.KeycloakMain;

public final class QuarkusConfigProbe {
    public static void main(String[] args) {
        List<ThreadLocal<Integer>> locals = new ArrayList<>();
        for (int i = 0; i < 64; i++) {
            ThreadLocal<Integer> local = new ThreadLocal<>();
            local.set(i);
            locals.add(local);
        }
        int mismatches = 0;
        for (int i = 0; i < locals.size(); i++) {
            if (!Integer.valueOf(i).equals(locals.get(i).get())) {
                mismatches++;
            }
        }
        ThreadLocal<LinkedHashSet<String>> recursions = NestedPropertyMappingInterceptor.recursions;
        ThreadLocal<LinkedHashSet<String>> rootLocal = new ThreadLocal<>();
        LinkedHashSet<String> rootValue = new LinkedHashSet<>();
        rootValue.add("root");
        rootLocal.set(rootValue);
        LinkedHashSet<String> recursionSet = new LinkedHashSet<>();
        recursionSet.add("quarkus.otel.enabled");
        recursionSet.add("quarkus.otel.traces.enabled");
        recursionSet.add("kc.tracing-enabled");
        recursionSet.remove("kc.tracing-enabled");
        System.out.println("threadLocalMismatches=" + mismatches + " rootLocalPreserved=" + (rootLocal.get() == rootValue) + " recursionSet=" + recursionSet + " recursionsInitial=" + recursions.get());
        Properties baseline = (Properties) System.getProperties().clone();
        KeycloakMain.reset(baseline);
        Environment.setHomeDir(Paths.get("src/test/resources/"));
        KcEnvConfigSource.ENV_OVERRIDE.clear();
        KcEnvConfigSource.ENV_OVERRIDE.put("KC_TRACING_ENABLED", "true");
        KcEnvConfigSource.ENV_OVERRIDE.put("KC_DB_PASSWORD", "from-kc");
        KcEnvConfigSource.ENV_OVERRIDE.put("KCRAW_DB_PASSWORD", "from-kcraw");
        Map<String, String> copiedEnv = new HashMap<>(System.getenv());
        copiedEnv.putAll(KcEnvConfigSource.ENV_OVERRIDE);
        System.out.println("env.copy size=" + copiedEnv.size()
                + " kc=" + copiedEnv.containsKey("KC_DB_PASSWORD")
                + " raw=" + copiedEnv.containsKey("KCRAW_DB_PASSWORD"));
        try {
            KcEnvConfigSource.getConfigSources();
            System.out.println("env.conflict=not-thrown");
        } catch (Exception e) {
            System.out.println("env.conflict=" + e.getClass().getName() + ":" + e.getMessage());
        }
        KcEnvConfigSource.ENV_OVERRIDE.clear();
        KcEnvConfigSource.ENV_OVERRIDE.put("KC_TRACING_ENABLED", "true");
        Configuration.resetConfig();

        System.out.println("system.java.home=" + System.getProperty("java.home"));
        System.out.println("kc.tracing-enabled=" + Configuration.getConfigValue("kc.tracing-enabled").getValue());
        System.out.println("quarkus.otel.traces.enabled=" + Configuration.getConfigValue("quarkus.otel.traces.enabled").getValue());
        System.out.println("optional.quarkus.otel.traces.enabled=" + Configuration.getOptionalValue("quarkus.otel.traces.enabled").orElse("<empty>"));
        System.out.println("config.isTrue.quarkus.otel.traces.enabled=" + Configuration.isTrue("quarkus.otel.traces.enabled"));
        System.out.println("quarkus.otel.enabled=" + Configuration.getConfigValue("quarkus.otel.enabled").getValue());
        System.out.println("cache.kc=" + Configuration.getConfigValue("kc.spi-cache-embedded--default--config-file").getValue());
        System.out.println("cache.legacy=" + Configuration.getConfigValue("spi-cache-embedded-default-config-file").getValue());
        System.out.println("propertyNamesKnownSize=" + Configuration.getConfig().getPropertyNames().spliterator().getExactSizeIfKnown());
        try (FileInputStream in = new FileInputStream("src/test/resources/conf/keystore")) {
            KeyStore store = KeyStore.getInstance("PKCS12");
            store.load(in, "secret".toCharArray());
            System.out.println("keystore.alias=" + store.containsAlias("my.secret") + " key=" + (store.getKey("my.secret", "secret".toCharArray()) != null));
        } catch (Exception e) {
            System.out.println("keystore.error=" + e);
        }
        try {
            KeycloakMain.reset(baseline);
            Environment.setHomeDir(Paths.get("src/test/resources/"));
            KcEnvConfigSource.ENV_OVERRIDE.clear();
            Configuration.resetConfig();
            ConfigArgsConfigSource.setCliArgs("");
            System.clearProperty(org.keycloak.common.util.Environment.PROFILE);
            Class<?> test = Class.forName("org.keycloak.quarkus.runtime.configuration.ConfigurationTest");
            java.lang.reflect.Method scopeFactory = test.getDeclaredMethod("cacheEmbeddedConfiguration");
            scopeFactory.setAccessible(true);
            Object scope = scopeFactory.invoke(null);
            java.lang.reflect.Method get = scope.getClass().getMethod("get", String.class);
            System.out.println("cluster.config=" + get.invoke(scope, "configFile"));
        } catch (Exception e) {
            System.out.println("cluster.error=" + e);
        }
    }
}
