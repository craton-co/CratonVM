import java.io.File;
import java.util.ArrayList;
import java.util.Enumeration;
import java.util.List;
import java.util.zip.ZipEntry;
import java.util.zip.ZipFile;
import org.springframework.core.type.AnnotationMetadata;

/**
 * Local reproducer for the JIT-only regression that kills Spring
 * configuration-class parsing.
 *
 * <p>The suite stack is
 *
 * <pre>
 * ConfigurationClassParser.retrieveBeanMethodMetadata(ConfigurationClassParser.java:459)
 *   -> StandardAnnotationMetadata.getAnnotatedMethods(StandardAnnotationMetadata.java:148)
 *     -> StandardAnnotationMetadata.isAnnotatedMethod(:168)
 *       -> AnnotatedElementUtils.isAnnotated(:232)     <-- NPE, getAnnotations() was null
 * </pre>
 *
 * <p>A hand-written stand-in class does NOT reproduce it — the fault needs the
 * volume and variety of real types Spring walks. So this drives the same entry
 * point (`getAnnotatedMethods`) over every class it can load out of the Spring
 * jars on the classpath, which is the closest thing to
 * `ConfigurationClassParser` that runs without the Spring Boot test fixture.
 *
 * <p>Reports the first failure with the class and the message, so a bisect step
 * is one short run.
 *
 * <pre>
 * cratonvm --java-home &lt;jdk&gt; -cp &lt;probe&gt;;&lt;spring jars&gt; SpringConfigParseProbe [rounds]
 * </pre>
 */
public class SpringConfigParseProbe {

    private static List<String> classNamesFrom(String jarPath) {
        List<String> out = new ArrayList<>();
        try (ZipFile zf = new ZipFile(jarPath)) {
            Enumeration<? extends ZipEntry> en = zf.entries();
            while (en.hasMoreElements()) {
                String n = en.nextElement().getName();
                if (n.endsWith(".class") && !n.contains("module-info")) {
                    out.add(n.substring(0, n.length() - 6).replace('/', '.'));
                }
            }
        } catch (Exception e) {
            // A jar we cannot open contributes nothing; the corpus size check
            // below is what stops that from passing vacuously.
        }
        return out;
    }

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 6;

        List<String> names = new ArrayList<>();
        for (String entry : System.getProperty("java.class.path").split(File.pathSeparator)) {
            if (entry.endsWith(".jar") && entry.contains("spring")) {
                names.addAll(classNamesFrom(entry));
            }
        }
        System.out.println("candidate classes: " + names.size());
        if (names.size() < 200) {
            System.out.println("SPRINGPARSE FAIL (corpus too small to be meaningful)");
            System.exit(2);
        }

        List<Class<?>> loaded = new ArrayList<>();
        for (String n : names) {
            try {
                loaded.add(Class.forName(n, false, SpringConfigParseProbe.class.getClassLoader()));
            } catch (Throwable ignored) {
                // Optional dependencies are absent by design; skip them.
            }
        }
        System.out.println("loaded classes   : " + loaded.size());
        if (loaded.size() < 200) {
            System.out.println("SPRINGPARSE FAIL (loaded too few to be meaningful)");
            System.exit(2);
        }

        long walked = 0;
        long methodsSeen = 0;
        long failures = 0;
        String firstFailure = null;

        for (int r = 0; r < rounds; r++) {
            for (Class<?> c : loaded) {
                try {
                    AnnotationMetadata md = AnnotationMetadata.introspect(c);
                    // The exact call `retrieveBeanMethodMetadata` makes.
                    methodsSeen += md.getAnnotatedMethods("org.springframework.context.annotation.Bean").size();
                    methodsSeen += md.getAnnotatedMethods("org.springframework.core.annotation.Order").size();
                    walked++;
                } catch (NullPointerException npe) {
                    failures++;
                    if (firstFailure == null) {
                        firstFailure = c.getName() + " round=" + r + " : " + npe.getMessage();
                        System.out.println("  FIRST FAILURE " + firstFailure);
                    }
                } catch (Throwable t) {
                    // Anything other than the NPE under investigation is not
                    // this defect; a class whose supertypes are absent throws
                    // TypeNotPresentException and similar here.
                }
            }
        }

        System.out.println("classes walked   : " + walked);
        System.out.println("annotated methods: " + methodsSeen);
        System.out.println("NPE failures     : " + failures);
        boolean ok = failures == 0 && walked > 0;
        System.out.println(ok ? "SPRINGPARSE PASS" : "SPRINGPARSE FAIL");
        if (!ok) {
            System.exit(1);
        }
    }
}
