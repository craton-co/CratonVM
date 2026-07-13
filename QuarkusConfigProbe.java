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
import org.keycloak.quarkus.runtime.configuration.mappers.PropertyMappers;
import org.junit.runner.Description;
import org.junit.runner.JUnitCore;
import org.junit.runner.Result;
import org.junit.Test;
import org.junit.runner.notification.Failure;
import org.junit.runner.notification.RunListener;
import java.util.ServiceConfigurationError;

public final class QuarkusConfigProbe {
    public static void main(String[] args) {
        if (args.length == 1 && args[0].equals("observe")) {
            observeConfigurationTest();
            return;
        }
        if (args.length == 1 && args[0].equals("mappers")) {
            Properties baseline = (Properties) System.getProperties().clone();
            KeycloakMain.reset(baseline);
            Environment.setHomeDir(Paths.get("src/test/resources/"));
            KcEnvConfigSource.ENV_OVERRIDE.clear();
            Configuration.resetConfig();
            Configuration.getConfigValue("kc.tracing-enabled");
            int[] count = {0};
            PropertyMappers.getMappers().forEach(mapper -> {
                if (count[0] < 12) {
                    System.out.println("mapper[" + count[0] + "]=" + mapper.getClass().getName());
                }
                count[0]++;
            });
            System.out.println("mapper.count=" + count[0]);
            return;
        }
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

    private static void observeConfigurationTest() {
        JUnitCore core = new JUnitCore();
        core.addListener(new RunListener() {
            @Override
            public void testStarted(Description description) {
                String name = description.getMethodName();
                if ("testClusterConfig".equals(name) || "testRawAndKcConflictThrowsError".equals(name)
                        || "testResolveTransformedValue".equals(name)) {
                    System.out.println("observer.start=" + name
                            + " cli=" + System.getProperty("kc.config.args")
                            + " envSize=" + KcEnvConfigSource.ENV_OVERRIDE.size()
                            + " envKc=" + KcEnvConfigSource.ENV_OVERRIDE.containsKey("KC_DB_PASSWORD")
                            + " envRaw=" + KcEnvConfigSource.ENV_OVERRIDE.containsKey("KCRAW_DB_PASSWORD"));
                    if ("testClusterConfig".equals(name)) {
                        String home = Environment.getHomeDir().orElseThrow().toString();
                        System.out.println("observer.cluster home=" + home
                                + " joined=" + Paths.get(home, "conf", "cache-ispn.xml"));
                    }
                    if ("testResolveTransformedValue".equals(name)) {
                        int[] counts = new int[2];
                        for (Object mapper : PropertyMappers.getMappers()) {
                            if (mapper instanceof Object) counts[0]++;
                            else counts[1]++;
                        }
                        System.out.println("observer.mappers objects=" + counts[0] + " unexpected=" + counts[1]);
                    }
                }
            }

            @Override
            public void testFailure(Failure failure) {
                System.out.println("observer.failure=" + failure.getDescription().getMethodName()
                        + " message=" + failure.getMessage());
            }
        });
        Result result = core.run(ObservedConfigurationTest.class);
        System.out.println("observer.result=" + result.getRunCount() + "/" + result.getFailureCount());
    }

    public static class ObservedConfigurationTest extends ConfigurationTest {
        @Override
        @Test
        public void testClusterConfig() {
            try {
                System.out.println("observer.cluster first=" + cacheValue());
                System.clearProperty(org.keycloak.common.util.Environment.PROFILE);
                ConfigArgsConfigSource.setCliArgs("--cache-config-file=cluster-foo.xml");
                System.out.println("observer.cluster explicit=" + cacheValue());
                System.setProperty(org.keycloak.common.util.Environment.PROFILE, "dev");
                System.out.println("observer.cluster devExplicit=" + cacheValue());
                ConfigArgsConfigSource.setCliArgs("");
                System.out.println("observer.cluster devDefault=" + cacheValue());
            } catch (Exception e) {
                throw new AssertionError(e);
            }
        }

        private static Object cacheValue() throws ReflectiveOperationException {
            java.lang.reflect.Method scopeFactory = ConfigurationTest.class.getDeclaredMethod("cacheEmbeddedConfiguration");
            scopeFactory.setAccessible(true);
            Object scope = scopeFactory.invoke(null);
            return scope.getClass().getMethod("get", String.class).invoke(scope, "configFile");
        }

        @Override
        @Test
        public void testRawAndKcConflictThrowsError() {
            putEnvVar("KC_DB_PASSWORD", "from-kc");
            putEnvVar("KCRAW_DB_PASSWORD", "from-kcraw");
            System.out.println("observer.raw afterPut size=" + KcEnvConfigSource.ENV_OVERRIDE.size()
                    + " kc=" + KcEnvConfigSource.ENV_OVERRIDE.get("KC_DB_PASSWORD")
                    + " raw=" + KcEnvConfigSource.ENV_OVERRIDE.get("KCRAW_DB_PASSWORD"));
            try {
                KcEnvConfigSource.getConfigSources();
                System.out.println("observer.raw directSources=not-thrown");
            } catch (Exception e) {
                System.out.println("observer.raw directSources=" + e);
            }
            System.out.println("observer.raw providerBefore=" + providerSources());
            try {
                createConfig();
                System.out.println("observer.raw providerAfter=" + providerSources());
                org.junit.Assert.fail("Expected error for conflicting KC_ and KCRAW_ env vars");
            } catch (ServiceConfigurationError e) {
                System.out.println("observer.raw caught=" + e.getCause());
            }
        }
    }

    private static String providerSources() {
        try {
            java.lang.reflect.Field field = KeycloakConfigSourceProvider.class.getDeclaredField("CONFIG_SOURCES");
            field.setAccessible(true);
            List<?> sources = (List<?>) field.get(null);
            return "size=" + sources.size() + " empty=" + sources.isEmpty();
        } catch (ReflectiveOperationException e) {
            return e.toString();
        }
    }
}
