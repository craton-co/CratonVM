import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.FileInputStream;
import java.io.IOException;
import java.io.InputStream;
import java.lang.annotation.Annotation;
import java.lang.reflect.Method;

// Regression probe for the "Class-valued annotation element / annotation type
// resolves to null" cluster
// (docs/known-issues/springboot/annotation-class-element-resolves-null-cluster-20260805.md).
//
// Every check below is a place where a real framework calls a method on the
// result of `Annotation.annotationType()` or of a Class-valued annotation
// element WITHOUT a null check:
//
//   S1  Spring  AnnotationsScanner.isIgnorable(annotation.annotationType())
//   S2  JUnit   AnnotationUtils.findRepeatableAnnotations — walks a @Repeatable
//               container's value() array and calls annotationType() on each entry
//   S3  Spring  AnnotationTypeMappings.addMetaAnnotationsToQueue — getDeclaredAnnotations()
//               on the ANNOTATION TYPE itself, then annotationType() on each meta-annotation
//   S4  ByteBuddy ForFieldBinding.bind — declaringType(annotation).represents(void.class)
//               on an OMITTED Class-valued element whose default is `void.class`
//   S5  all of the above, for classes defined by a child loader that also
//               redefines the annotation types (so the annotation type name has
//               more than one definition and the loader-blind global lookup is
//               ambiguous)
//
// Prints `SUMMARY: S1=... S2=... S3=... S4=... S5=...`. HotSpot passes all of
// them; run it under both VMs to diff.
public class AnnClassProbe {

    /** Redefines every `Acp*` class under itself, so each name has TWO definitions. */
    static class DuplicatingClassLoader extends ClassLoader {
        DuplicatingClassLoader(ClassLoader parent) {
            super(parent);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (name.startsWith("Acp")) {
                Class<?> already = findLoadedClass(name);
                if (already != null) {
                    return already;
                }
                byte[] bytes = readBytes(name);
                if (bytes != null) {
                    Class<?> c = defineClass(name, bytes, 0, bytes.length);
                    if (resolve) {
                        resolveClass(c);
                    }
                    return c;
                }
            }
            return super.loadClass(name, resolve);
        }

        private byte[] readBytes(String name) {
            String res = name.replace('.', '/') + ".class";
            InputStream in = getParent() != null ? getParent().getResourceAsStream(res) : null;
            if (in == null) {
                in = getSystemResourceAsStream(res);
            }
            if (in == null) {
                String dir = System.getProperty("probe.dir");
                if (dir != null) {
                    try {
                        in = new FileInputStream(new File(dir, res));
                    } catch (IOException e) {
                        in = null;
                    }
                }
            }
            if (in == null) {
                return null;
            }
            try {
                ByteArrayOutputStream bos = new ByteArrayOutputStream();
                byte[] buf = new byte[4096];
                int n;
                while ((n = in.read(buf)) != -1) {
                    bos.write(buf, 0, n);
                }
                return bos.toByteArray();
            } catch (IOException e) {
                return null;
            } finally {
                try {
                    in.close();
                } catch (IOException ignore) {
                    // best effort
                }
            }
        }
    }

    /** Every element of `anns` must report a non-null annotationType(). */
    private static boolean typesAllPresent(String where, Annotation[] anns) {
        boolean ok = true;
        if (anns.length == 0) {
            System.out.println("  " + where + ": EMPTY [FAIL]");
            return false;
        }
        for (Annotation a : anns) {
            if (a == null) {
                System.out.println("  " + where + ": null ARRAY ELEMENT [FAIL]");
                ok = false;
                continue;
            }
            Class<? extends Annotation> t = a.annotationType();
            System.out.println("  " + where + ": " + (t == null ? "annotationType()=NULL [FAIL]" : t.getName()));
            if (t == null) {
                ok = false;
            }
        }
        return ok;
    }

    private static boolean checkDirect(Class<?> target) throws Exception {
        System.out.println("--- direct getDeclaredAnnotations() on " + target.getName());
        return typesAllPresent("class", target.getDeclaredAnnotations());
    }

    /**
     * The @Repeatable container's value() array — the exact array JUnit's
     * findRepeatableAnnotations recurses over.
     */
    private static boolean checkContainerEntries(Class<?> target) throws Exception {
        System.out.println("--- @Repeatable container value() entries on " + target.getName());
        boolean ok = true;
        for (Annotation a : target.getDeclaredAnnotations()) {
            Class<? extends Annotation> t = a.annotationType();
            if (t == null || !t.getName().equals("AcpTags")) {
                continue;
            }
            Method value = t.getMethod("value");
            Object[] entries = (Object[]) value.invoke(a);
            ok &= typesAllPresent("entry", (Annotation[]) entries);
            for (Object e : entries) {
                Annotation tag = (Annotation) e;
                Class<? extends Annotation> tt = tag.annotationType();
                if (tt == null) {
                    ok = false;
                    continue;
                }
                Object type = tt.getMethod("type").invoke(tag);
                System.out.println("  entry type() = " + type);
                if (type == null) {
                    ok = false;
                }
            }
            return ok;
        }
        System.out.println("  container NOT FOUND [FAIL]");
        return false;
    }

    /** getDeclaredAnnotations() on the annotation TYPE (meta-annotation walk). */
    private static boolean checkMetaAnnotations(ClassLoader loader) throws Exception {
        Class<?> tag = Class.forName("AcpTag", true, loader);
        System.out.println("--- meta-annotations on " + tag.getName());
        return typesAllPresent("meta", tag.getDeclaredAnnotations());
    }

    /** An OMITTED Class-valued element whose declared default is `void.class`. */
    private static boolean checkVoidDefault(Class<?> target) throws Exception {
        System.out.println("--- omitted Class element with `void.class` default");
        Method m = target.getMethod("withFieldValue");
        Annotation[] anns = m.getDeclaredAnnotations();
        if (!typesAllPresent("method", anns)) {
            return false;
        }
        for (Annotation a : anns) {
            Class<? extends Annotation> t = a.annotationType();
            if (t == null || !t.getName().equals("AcpFieldValue")) {
                continue;
            }
            Object declaringType = t.getMethod("declaringType").invoke(a);
            System.out.println("  declaringType() = " + declaringType);
            boolean ok = (declaringType == Void.TYPE);
            if (!ok) {
                System.out.println("  expected void.class [FAIL]");
            }
            return ok;
        }
        System.out.println("  @AcpFieldValue NOT FOUND [FAIL]");
        return false;
    }

    private static boolean runAll(String label, ClassLoader loader) throws Exception {
        System.out.println();
        System.out.println("=== " + label + " ===");
        Class<?> target = Class.forName("AcpTarget", true, loader);
        boolean ok = checkDirect(target);
        ok &= checkContainerEntries(target);
        ok &= checkMetaAnnotations(loader);
        ok &= checkVoidDefault(target);
        return ok;
    }

    public static void main(String[] args) throws Exception {
        ClassLoader app = AnnClassProbe.class.getClassLoader();

        Class<?> target = Class.forName("AcpTarget", true, app);
        boolean s1 = checkDirect(target);
        boolean s2 = checkContainerEntries(target);
        boolean s3 = checkMetaAnnotations(app);
        boolean s4 = checkVoidDefault(target);

        // The child loader defines its OWN copies of every Acp* class, so each
        // annotation type name now has two definitions and any loader-blind
        // "unique class by name" lookup is ambiguous.
        boolean s5;
        try {
            s5 = runAll("child loader (duplicate definitions)", new DuplicatingClassLoader(app));
        } catch (Throwable t) {
            System.out.println("child-loader pass THREW " + t);
            s5 = false;
        }

        System.out.println();
        System.out.println("SUMMARY: S1=" + (s1 ? "PASS" : "FAIL")
                + " S2=" + (s2 ? "PASS" : "FAIL")
                + " S3=" + (s3 ? "PASS" : "FAIL")
                + " S4=" + (s4 ? "PASS" : "FAIL")
                + " S5=" + (s5 ? "PASS" : "FAIL"));
    }
}
