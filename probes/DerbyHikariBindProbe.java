package org.springframework.boot.devtools.autoconfigure;

import javax.sql.DataSource;

import org.springframework.boot.jdbc.autoconfigure.DataSourceAutoConfiguration;
import org.springframework.boot.test.util.TestPropertyValues;
import org.springframework.context.annotation.AnnotationConfigApplicationContext;

/**
 * Reproduces DevToolsPooledDataSourceAutoConfigurationTests.inMemoryDerbyIsShutdown's
 * context creation on the calling thread, with the full cause chain printed.
 *
 * The test itself builds the context on a worker thread and only asserts the
 * result is non-null, so the actual failure — a
 * ConfigurationPropertiesBindException binding spring.datasource.hikari to
 * HikariDataSource — never reaches the report.
 *
 * Lives in the production package so it can see the package-private
 * DataSourceSpyConfiguration the test registers.
 */
public class DerbyHikariBindProbe {

    public static void main(String[] args) {
        String driver = args.length > 0 ? args[0] : "org.apache.derby.jdbc.EmbeddedDriver";
        String url = args.length > 1 ? args[1] : "jdbc:derby:memory:test;create=true";
        System.out.println("driver = " + driver);
        System.out.println("url    = " + url);

        AnnotationConfigApplicationContext context = new AnnotationConfigApplicationContext();
        context.register(DataSourceAutoConfiguration.class,
                AbstractDevToolsDataSourceAutoConfigurationTests.DataSourceSpyConfiguration.class);
        context.register(DevToolsDataSourceAutoConfiguration.class);
        TestPropertyValues.of("spring.datasource.driver-class-name:" + driver).applyTo(context);
        TestPropertyValues.of("spring.datasource.url:" + url).applyTo(context);
        try {
            context.refresh();
            DataSource ds = context.getBean(DataSource.class);
            System.out.println("refresh OK, dataSource = " + ds.getClass().getName());
            context.close();
            System.out.println("PROBE PASS");
        }
        catch (Throwable ex) {
            System.out.println("PROBE FAIL — refresh threw");
            int depth = 0;
            for (Throwable t = ex; t != null && depth < 12; t = t.getCause(), depth++) {
                System.out.println("  [" + depth + "] " + t.getClass().getName() + ": " + t.getMessage());
                StackTraceElement[] st = t.getStackTrace();
                for (int i = 0; i < Math.min(st.length, 12); i++) {
                    System.out.println("        at " + st[i]);
                }
                if (t.getCause() == t) {
                    break;
                }
            }
            System.exit(1);
        }
    }
}
