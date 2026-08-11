import java.io.File;
import java.lang.reflect.Method;
import java.net.URL;
import java.net.URLClassLoader;
import java.util.ArrayList;
import java.util.List;

/**
 * Initialise Mockito's inline mock maker in TWO class-loader worlds, the way
 * Spring Boot's {@code ModifiedClassPathClassLoader} does when a test class
 * mixes {@code @ClassPathExclusions}-annotated methods with plain ones.
 *
 * {@code HikariDataSourceConfigurationTests} does exactly that: the excluded-
 * classpath method mocks a {@code Connection} in the isolated world and a plain
 * method mocks one on the application loader. On CratonVM the SECOND world dies
 * with {@code Could not initialize plugin: interface org.mockito.plugins.MockMaker}
 * whose root cause is Byte Buddy's {@code JavaDispatcher} throwing
 * {@code No proxy target found for ... Executable.isInstance(Object)} — a
 * {@code Map<Method, Dispatcher>} lookup that missed. HotSpot initialises both.
 *
 * The isolated loader is parented to the PLATFORM loader, matching
 * {@code ModifiedClassPathClassLoader}, so it must define its own copies of
 * Mockito and Byte Buddy rather than delegating to the application loader.
 *
 * Usage: {@code MockitoTwoWorldProbe [appFirst|isolatedFirst]}
 *   — the classpath entries are taken from {@code java.class.path}.
 */
public final class MockitoTwoWorldProbe {

    public interface Sample {
        int compute(String input);
    }

    public static void main(String[] args) throws Exception {
        boolean appFirst = args.length == 0 || "appFirst".equals(args[0]);

        if (appFirst) {
            report("world-app", mockOnAppLoader());
            report("world-isolated", mockOnIsolatedLoader());
        } else {
            report("world-isolated", mockOnIsolatedLoader());
            report("world-app", mockOnAppLoader());
        }
    }

    private static void report(String label, Throwable failure) {
        if (failure == null) {
            System.out.println("PROBE " + label + " mock=OK");
            return;
        }
        System.out.println("PROBE " + label + " mock=FAILED " + failure);
        for (Throwable t = failure.getCause(); t != null; t = t.getCause()) {
            System.out.println("PROBE " + label + "   caused-by " + t);
        }
    }

    private static Throwable mockOnAppLoader() {
        try {
            Object mock = org.mockito.Mockito.mock(Sample.class);
            System.out.println("PROBE app mock class = " + mock.getClass().getName());
            return null;
        }
        catch (Throwable t) {
            return t;
        }
    }

    private static Throwable mockOnIsolatedLoader() {
        try {
            List<URL> urls = new ArrayList<>();
            for (String entry : System.getProperty("java.class.path")
                    .split(File.pathSeparator)) {
                if (!entry.isEmpty()) {
                    urls.add(new File(entry).toURI().toURL());
                }
            }
            ClassLoader platform = ClassLoader.getPlatformClassLoader();
            URLClassLoader isolated = new URLClassLoader("isolated",
                    urls.toArray(new URL[0]), platform);

            Class<?> sample = Class.forName(MockitoTwoWorldProbe.class.getName() + "$Sample",
                    false, isolated);
            Class<?> mockito = Class.forName("org.mockito.Mockito", true, isolated);
            System.out.println("PROBE isolated sampleLoaderIsChild="
                    + (sample.getClassLoader() == isolated)
                    + " mockitoLoaderIsChild=" + (mockito.getClassLoader() == isolated));
            Method mock = mockito.getMethod("mock", Class.class);
            Object result = mock.invoke(null, sample);
            System.out.println("PROBE isolated mock class = " + result.getClass().getName());
            isolated.close();
            return null;
        }
        catch (Throwable t) {
            return t;
        }
    }
}
