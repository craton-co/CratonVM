import java.io.*;
import java.lang.annotation.Annotation;
import java.lang.reflect.Method;

// Mirrors Spring's AnnotationIntrospectionFailureTests:
//   - FilteringClassLoader (OverridingClassLoader-style): redefines test classes
//     under itself, and throws ClassNotFoundException for any "*Filtered*" type.
//   - @AnnProbeExampleAnnotation(AnnProbeFilteredType.class) on AnnProbeWithAnnotation.
//   - Reading the Class-valued value() must throw TypeNotPresentException, because
//     the annotation TYPE's defining loader (the filtering loader) refuses to load
//     AnnProbeFilteredType.
public class AnnProbe {

    static class FilteringClassLoader extends ClassLoader {
        FilteringClassLoader(ClassLoader parent) { super(parent); }

        private boolean isEligible(String name) {
            return name.startsWith("AnnProbe");
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (name.contains("Filtered")) {
                throw new ClassNotFoundException(name + " (filtered)");
            }
            if (isEligible(name)) {
                Class<?> already = findLoadedClass(name);
                if (already != null) return already;
                byte[] bytes = readBytes(name);
                if (bytes != null) {
                    Class<?> c = defineClass(name, bytes, 0, bytes.length);
                    if (resolve) resolveClass(c);
                    return c;
                }
            }
            return super.loadClass(name, resolve);
        }

        private byte[] readBytes(String name) {
            String res = name.replace('.', '/') + ".class";
            InputStream in = getParent() != null ? getParent().getResourceAsStream(res) : null;
            if (in == null) in = getSystemResourceAsStream(res);
            if (in == null) {
                String dir = System.getProperty("probe.dir");
                if (dir != null) {
                    try { in = new FileInputStream(new File(dir, res)); } catch (IOException e) { in = null; }
                }
            }
            if (in == null) return null;
            try {
                ByteArrayOutputStream bos = new ByteArrayOutputStream();
                byte[] buf = new byte[4096];
                int n;
                while ((n = in.read(buf)) != -1) bos.write(buf, 0, n);
                return bos.toByteArray();
            } catch (IOException e) {
                return null;
            } finally {
                try { in.close(); } catch (IOException ignore) {}
            }
        }
    }

    public static void main(String[] args) throws Exception {
        FilteringClassLoader fcl = new FilteringClassLoader(AnnProbe.class.getClassLoader());

        System.out.println("=== STEP 1: load WithAnnotation via filtering loader ===");
        Class<?> withAnn = Class.forName("AnnProbeWithAnnotation", false, fcl);
        boolean s1 = (withAnn.getClassLoader() == fcl);
        System.out.println("withAnn loader   = " + withAnn.getClassLoader());
        System.out.println("withAnn==fcl     = " + s1);

        System.out.println();
        System.out.println("=== STEP 2: getAnnotations() (proxy creation must NOT throw) ===");
        Annotation[] anns;
        try {
            anns = withAnn.getAnnotations();
            System.out.println("getAnnotations() count = " + anns.length + "  [OK - did not throw]");
        } catch (Throwable t) {
            System.out.println("getAnnotations() THREW " + t.getClass().getName() + ": " + t.getMessage()
                    + "  [PREMATURE - should defer to value()]");
            System.out.println("SUMMARY: S1=" + (s1?"PASS":"FAIL") + " S2=FAIL S3=SKIP");
            return;
        }
        if (anns.length == 0) {
            System.out.println("no annotations found  [FAIL]");
            System.out.println("SUMMARY: S1=" + (s1?"PASS":"FAIL") + " S2=FAIL S3=SKIP");
            return;
        }
        Annotation annotation = anns[0];
        Class<?> annType = annotation.annotationType();
        System.out.println("annType          = " + annType.getName());
        System.out.println("annType loader   = " + annType.getClassLoader());

        System.out.println();
        System.out.println("=== STEP 3: invoke value() — expect TypeNotPresentException ===");
        Method value = annType.getMethod("value");
        boolean s3;
        try {
            Object result = value.invoke(annotation);
            System.out.println("value() returned " + result + "  [FAIL - filter bypassed]");
            s3 = false;
        } catch (java.lang.reflect.InvocationTargetException ite) {
            Throwable cause = ite.getCause();
            System.out.println("value() threw (via reflection) " + cause.getClass().getName() + ": " + cause.getMessage());
            boolean isTnpe = (cause instanceof TypeNotPresentException);
            boolean causeCnfe = cause.getCause() instanceof ClassNotFoundException;
            System.out.println("  TypeNotPresentException? " + isTnpe + "  cause is CNFE? " + causeCnfe);
            s3 = isTnpe;
        } catch (TypeNotPresentException tnpe) {
            System.out.println("value() threw TypeNotPresentException: " + tnpe.getMessage()
                    + "  cause=" + (tnpe.getCause() == null ? "null" : tnpe.getCause().getClass().getName()));
            s3 = true;
        }

        System.out.println();
        System.out.println("=== STEP 4: resolvable Class member via filtering loader must NOT throw ===");
        Class<?> withOk = Class.forName("AnnProbeWithOk", false, fcl);
        Annotation okAnn = withOk.getAnnotations()[0];
        Method okValue = okAnn.annotationType().getMethod("value");
        boolean s4;
        try {
            Object r = okValue.invoke(okAnn);
            s4 = (r == String.class);
            System.out.println("value() returned " + r + "  " + (s4 ? "[PASS]" : "[FAIL - wrong class]"));
        } catch (Throwable t) {
            Throwable c = (t instanceof java.lang.reflect.InvocationTargetException) ? t.getCause() : t;
            System.out.println("value() THREW " + c.getClass().getName() + "  [FAIL - false TypeNotPresent]");
            s4 = false;
        }

        System.out.println();
        System.out.println("=== STEP 5: resolvable child-eligible Class member -> loaded by the child loader ===");
        // Mirrors MergedAnnotationClassLoaderTests.synthesizedUsesCorrectClassLoader:
        // the Class value resolves through the declaring class's (filtering) loader,
        // which redefines AnnProbeRefType, so value().getClassLoader() == fcl.
        Class<?> withRef = Class.forName("AnnProbeWithRef", false, fcl);
        Annotation refAnn = withRef.getAnnotations()[0];
        Method refValue = refAnn.annotationType().getMethod("value");
        boolean s5;
        try {
            Class<?> rv = (Class<?>) refValue.invoke(refAnn);
            s5 = (rv.getClassLoader() == fcl);
            System.out.println("value()=" + rv.getName() + " loadedBy=" + rv.getClassLoader()
                    + "  " + (s5 ? "[PASS]" : "[FAIL - wrong loader]"));
        } catch (Throwable t) {
            Throwable c = (t instanceof java.lang.reflect.InvocationTargetException) ? t.getCause() : t;
            System.out.println("value() THREW " + c.getClass().getName() + "  [FAIL]");
            s5 = false;
        }

        System.out.println();
        System.out.println("=== STEP 6: the SAME member read from an APP-loaded holder stays in the app world ===");
        // The other direction of STEP 5, and the shape that dropped log4j's
        // whole <Logger> configuration: STEP 5 left AnnProbeRefType defined
        // ONLY by the child loader, so a loader-blind "who has this name"
        // lookup answers with the child's copy. An app-loaded holder must
        // still get the APPLICATION copy — its own loader can load the class
        // itself, and handing it the child's builds a world that exists on no
        // real JVM: an app-world caller reaching a child-world callee, and a
        // ClassCastException at the first honest checkcast.
        ClassLoader app = AnnProbe.class.getClassLoader();
        Class<?> appWithRef = Class.forName("AnnProbeWithRef", false, app);
        Annotation appRefAnn = appWithRef.getAnnotations()[0];
        Method appRefValue = appRefAnn.annotationType().getMethod("value");
        boolean s6;
        try {
            Class<?> rv = (Class<?>) appRefValue.invoke(appRefAnn);
            s6 = (rv.getClassLoader() == app);
            System.out.println("holder loadedBy=" + appWithRef.getClassLoader()
                    + " value()=" + rv.getName() + " loadedBy=" + rv.getClassLoader()
                    + "  " + (s6 ? "[PASS]" : "[FAIL - app holder handed the child loader's copy]"));
        } catch (Throwable t) {
            Throwable c = (t instanceof java.lang.reflect.InvocationTargetException) ? t.getCause() : t;
            System.out.println("value() THREW " + c.getClass().getName() + "  [FAIL]");
            s6 = false;
        }

        System.out.println();
        System.out.println("SUMMARY: S1=" + (s1?"PASS":"FAIL") + " S2=PASS S3=" + (s3?"PASS":"FAIL")
                + " S4=" + (s4?"PASS":"FAIL") + " S5=" + (s5?"PASS":"FAIL")
                + " S6=" + (s6?"PASS":"FAIL"));
    }
}
