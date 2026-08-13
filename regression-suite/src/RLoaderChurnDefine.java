import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.util.ArrayList;
import java.util.List;

/**
 * The Tomcat webapp stop/start shape: MANY short-lived loaders, each defining
 * the SAME class name, in one process.
 *
 * WHY THIS EXISTS. `defineClass` refuses a duplicate definition by the same
 * loader (JVMS §5.3.5) — but only by the same LOADER OBJECT. CratonVM keys its
 * class store on `(loader_id, name)` where `loader_id` is a synthetic NAMESPACE
 * number, and two distinct `ClassLoader` objects can end up sharing one. When
 * that happens the backend reports "already defined" for a definition JVMS
 * permits, and the natives serve the existing mirror instead of surfacing it.
 *
 * That tolerance is not decoration. It was added for a measured failure: every
 * `TestEncodingDetector` sub-test starts and stops its own embedded Tomcat, and
 * after ~14 stop/start cycles in one process
 * `defineClass1(org/apache/catalina/loader/JdbcLeakPrevention)` began throwing,
 * cascading into `LifecycleException: A child container failed during stop` for
 * every later context. On HotSpot each `WebappClassLoader` is a different
 * loader and every one of those definitions is legal.
 *
 * So this vector holds the OTHER half of the duplicate-define rule down.
 * `RJdkDefineClass` pins the refusal; this one pins the permission, in the
 * shape that produced the incident: enough loaders, enough repetitions, and the
 * same handful of names throughout.
 *
 * ANTI-VACUITY. A run in which the namespace never collides would pass this
 * vector while testing nothing. `CRATONVM_DBG_DUPDEF=1` prints every
 * "already defined" verdict, so a maintainer can check the tolerance arm was
 * actually reached rather than assume it; the vector itself asserts the
 * OBSERVABLE consequence, which holds either way — every loader gets a class,
 * every class is that loader's own, and no two loaders share one.
 *
 * Determinism: no threads, no clock, no identity hashes. The class count and
 * the names are fixed.
 */
public class RLoaderChurnDefine {
    static int checks;

    /** Loaders per round, chosen to pass the ~14 that produced the incident. */
    static final int LOADERS = 40;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError("RLoaderChurnDefine: " + m);
        }
    }

    static byte[] tiny(String name) {
        ByteArrayOutputStream b = new ByteArrayOutputStream();
        DataOutputStream d = new DataOutputStream(b);
        try {
            d.writeInt(0xCAFEBABE);
            d.writeShort(0);
            d.writeShort(52);
            d.writeShort(5);
            d.writeByte(7); d.writeShort(2);
            d.writeByte(1); d.writeUTF(name);
            d.writeByte(7); d.writeShort(4);
            d.writeByte(1); d.writeUTF("java/lang/Object");
            d.writeShort(0x0031);
            d.writeShort(1);
            d.writeShort(3);
            d.writeShort(0); d.writeShort(0); d.writeShort(0); d.writeShort(0);
        } catch (Exception e) {
            throw new RuntimeException(e);
        }
        return b.toByteArray();
    }

    /** Stands in for `WebappClassLoader`: created, used once, discarded. */
    static final class Webapp extends ClassLoader {
        Webapp() {
            super(null);
        }

        Class<?> define(String n) {
            byte[] b = tiny(n);
            return defineClass(n, b, 0, b.length);
        }
    }

    /**
     * Forty loaders, each defining the same name. Every one must succeed and
     * every one must get its OWN class — that is what HotSpot does and what the
     * Tomcat incident needed.
     */
    static void manyLoadersOneName() {
        List<Class<?>> defined = new ArrayList<>();
        for (int i = 0; i < LOADERS; i++) {
            Webapp w = new Webapp();
            Class<?> c = w.define("Churn$Leak");
            check(c.getName().equals("Churn$Leak"), "round " + i + ": name");
            check(c.getClassLoader() == w, "round " + i + ": the defining loader owns it");
            check(c.getSuperclass() == Object.class, "round " + i + ": superclass");
            defined.add(c);
        }
        // Distinctness is the property a served-mirror shortcut would break, so
        // it is checked pairwise against every earlier round rather than only
        // against the previous one.
        for (int i = 0; i < defined.size(); i++) {
            for (int j = i + 1; j < defined.size(); j++) {
                check(defined.get(i) != defined.get(j),
                        "rounds " + i + " and " + j + " must be distinct classes");
            }
        }
        System.out.println("CK RLoaderChurnDefine loaders=" + defined.size());
    }

    /**
     * The same churn with a loader that defines SEVERAL names, and defines them
     * in a different order each round — the shape that makes a namespace
     * number, rather than a name, the thing that collides.
     */
    static void churnWithSeveralNames() {
        String[] names = { "Churn$A", "Churn$B", "Churn$C" };
        for (int i = 0; i < LOADERS; i++) {
            Webapp w = new Webapp();
            int rot = i % names.length;
            for (int k = 0; k < names.length; k++) {
                String n = names[(rot + k) % names.length];
                Class<?> c = w.define(n);
                check(c.getName().equals(n), "round " + i + ": " + n);
                check(c.getClassLoader() == w, "round " + i + ": " + n + " loader");
            }
            // …and this loader must now refuse ITS OWN name, in the same round.
            boolean refused = false;
            try {
                w.define(names[rot]);
            } catch (LinkageError e) {
                refused = true;
            }
            check(refused, "round " + i + ": the same loader must refuse its own duplicate");
        }
        System.out.println("CK RLoaderChurnDefine multiName=ok");
    }

    /**
     * A loader that outlives the churn keeps its class, and still refuses a
     * duplicate afterwards. A namespace recycled onto a later loader must not
     * silently hand this one's class away or make its own name re-definable.
     */
    static void aSurvivorKeepsItsClass() {
        Webapp survivor = new Webapp();
        Class<?> mine = survivor.define("Churn$Survivor");
        for (int i = 0; i < LOADERS; i++) {
            Webapp w = new Webapp();
            w.define("Churn$Survivor");
        }
        check(survivor.define("Churn$Other") != null, "the survivor can still define new names");
        boolean refused = false;
        try {
            survivor.define("Churn$Survivor");
        } catch (LinkageError e) {
            refused = true;
        }
        check(refused, "the survivor still refuses its own duplicate after the churn");
        check(mine.getClassLoader() == survivor, "…and still owns its original class");
        System.out.println("CK RLoaderChurnDefine survivor=ok");
    }

    /** Loaded through a bare {@code URLClassLoader} by `repeatLookupIsACacheHit`. */
    public static final class Echo {
        public static String tag() {
            return "echo";
        }
    }

    /**
     * Held by the APPLICATION loader, and used by
     * `anIsolatingLoaderCannotSeeTheAppLoadersClass` as the thing that must NOT
     * leak. Deliberately distinct from {@link Echo}: that one is loaded through
     * loaders whose URLs DO contain it, this one through loaders whose URLs do
     * not.
     */
    public static final class Sealed {
        public static String tag() {
            return "sealed";
        }
    }

    /**
     * A {@code URLClassLoader} SUBCLASS that adds nothing. The single
     * {@code extends} is the whole experiment: before W7-87 a subclass answered
     * correctly and a BARE {@code java.net.URLClassLoader} did not.
     */
    static final class SubUrlLoader extends java.net.URLClassLoader {
        SubUrlLoader(java.net.URL[] urls, ClassLoader parent) {
            super(urls, parent);
        }
    }

    /**
     * The THIRD half of the same rule, and the one this vector was missing: a
     * repeated LOOKUP is not a definition. {@code ClassLoader.loadClass} checks
     * {@code findLoadedClass} first and {@code Class.forName(name, initialize,
     * loader)} goes through the loader's initiating-classes record, so a second
     * call is a cache hit — not a second define, and not a {@code LinkageError}.
     *
     * Driven through a BARE {@code java.net.URLClassLoader} on purpose. Every
     * other section here uses a {@code ClassLoader} SUBCLASS, and a subclass took
     * a different route inside CratonVM: {@code java/net/URLClassLoader} is on
     * the built-in-loader-class list, so a bare instance was classified as a
     * built-in LOADER, could not see the class it had itself defined, and every
     * repeat lookup re-drove the define — which the duplicate rule above then
     * correctly refused. {@code IncompatibleClassChangeError: class X already
     * defined by user-defined(N) loader}, surfaced as {@code ClassFormatError}
     * out of {@code URLClassLoader.findClass}, on the SECOND
     * {@code Class.forName}.
     *
     * ANTI-VACUITY. Asserting that the second call "did not throw" would pass
     * against a VM that answers with a DIFFERENT {@code Class} object of the
     * same name, which is its own defect, so identity is asserted with
     * {@code ==}. The cross-loader half is asserted too: a second, independent
     * loader must get its OWN class, or "just return the global copy" would pass
     * everything above.
     *
     * The URLs are this run's own {@code java.class.path}, so nothing is written
     * and the section stays as deterministic as the rest of the vector. A null
     * parent forces the loader to define its own copy instead of delegating.
     */
    static void repeatLookupIsACacheHit() {
        String cp = System.getProperty("java.class.path");
        check(cp != null && !cp.isEmpty(),
                "java.class.path must be set, or this section tests nothing");
        String[] entries = cp.split(java.io.File.pathSeparator);
        java.net.URL[] urls = new java.net.URL[entries.length];
        for (int i = 0; i < entries.length; i++) {
            try {
                urls[i] = new java.io.File(entries[i]).toURI().toURL();
            } catch (java.net.MalformedURLException e) {
                throw new AssertionError("RLoaderChurnDefine: bad classpath entry " + entries[i]);
            }
        }
        String name = "RLoaderChurnDefine$Echo";

        java.net.URLClassLoader l1 = new java.net.URLClassLoader(urls, null);
        Class<?> a1;
        Class<?> a2;
        Class<?> a3;
        try {
            a1 = Class.forName(name, true, l1);
            a2 = Class.forName(name, true, l1);
            a3 = l1.loadClass(name);
        } catch (ClassNotFoundException e) {
            throw new AssertionError("RLoaderChurnDefine: bare URLClassLoader could not load "
                    + name + " from its own classpath: " + e);
        }
        check(a1 == a2, "a repeated Class.forName through one loader is a cache hit, "
                + "not a second definition");
        check(a1 == a3, "loadClass must answer with the same class Class.forName did");
        check(a1.getClassLoader() == l1, "the bare URLClassLoader defined its own copy");
        check(a1 != Echo.class, "…which is distinct from the application loader's copy");

        java.net.URLClassLoader l2 = new java.net.URLClassLoader(urls, null);
        Class<?> b1;
        Class<?> b2;
        try {
            b1 = Class.forName(name, true, l2);
            b2 = Class.forName(name, true, l2);
        } catch (ClassNotFoundException e) {
            throw new AssertionError("RLoaderChurnDefine: second loader could not load "
                    + name + ": " + e);
        }
        check(b1 == b2, "the second loader's repeat is a cache hit too");
        check(b1 != a1, "two loaders over one URL yield two classes");
        check(b1.getClassLoader() == l2, "…each owned by its own loader");

        // Both copies must actually work, so "isolated" cannot mean "broken".
        try {
            check("echo".equals(a1.getMethod("tag").invoke(null)), "loader 1's copy runs");
            check("echo".equals(b1.getMethod("tag").invoke(null)), "loader 2's copy runs");
        } catch (ReflectiveOperationException e) {
            throw new AssertionError("RLoaderChurnDefine: tag() failed: " + e);
        }
        System.out.println("CK RLoaderChurnDefine repeatLookup=ok");
    }

    /**
     * W7-87 — the FOURTH half, and the one that points the other way. The three
     * sections above all ask what a loader may DEFINE. This one asks what it may
     * SEE, and the answer is narrower than CratonVM used to give.
     *
     * {@code new URLClassLoader(urls, null)} — a private URL search path and a
     * BOOTSTRAP parent — is the standard idiom for a loader that deliberately
     * cannot reach the application classpath (Spring Boot's
     * {@code ModifiedClassPathClassLoader}, javax.tools harnesses, plugin
     * containers, `@ClassPathExclusions`). On HotSpot 25 it raises
     * {@code ClassNotFoundException} for an application class no matter what the
     * application loader already holds. CratonVM answered with the application
     * loader's class: `is_builtin_loader_class` lists
     * {@code java/net/URLClassLoader}, so `find_loaded_class_for_loader` took the
     * built-in branch and its GLOBAL FALLBACK, while the namespace allocator had
     * already classified the same object as user-defined. The isolation the
     * loader was constructed for did not exist.
     *
     * ANTI-VACUITY, three ways. (1) The application loader is asserted to be
     * holding {@link Sealed} FIRST — asking for a name nothing has loaded would
     * pass on a leaky VM too. (2) The same loader instance is asserted to still
     * delegate to bootstrap, so "isolated" cannot degenerate into "broken".
     * (3) A bare {@code URLClassLoader} parented to the APPLICATION loader is
     * asserted to still find it, which is the obvious over-correction: narrowing
     * the cache probe must not break parent-first delegation.
     *
     * The URL is {@code java.io.tmpdir}: a directory that certainly exists (so
     * the loader has a genuine, usable URL search path rather than the
     * degenerate empty one) and certainly does not contain this class. Nothing
     * is written.
     */
    static void anIsolatingLoaderCannotSeeTheAppLoadersClass() {
        String name = "RLoaderChurnDefine$Sealed";
        ClassLoader app = RLoaderChurnDefine.class.getClassLoader();
        check(Sealed.class.getClassLoader() == app,
                "the application loader must already hold " + name
                        + ", or there is nothing for an isolating loader to leak");

        java.net.URL[] urls;
        try {
            urls = new java.net.URL[] {
                new java.io.File(System.getProperty("java.io.tmpdir")).toURI().toURL()
            };
        } catch (java.net.MalformedURLException e) {
            throw new AssertionError("RLoaderChurnDefine: bad java.io.tmpdir URL: " + e);
        }

        java.net.URLClassLoader iso = new java.net.URLClassLoader(urls, null);
        try {
            Class<?> leaked = iso.loadClass(name);
            throw new AssertionError("RLoaderChurnDefine: new URLClassLoader(urls, null).loadClass("
                    + name + ") must raise ClassNotFoundException (HotSpot does); got " + leaked
                    + " owned by " + leaked.getClassLoader());
        } catch (ClassNotFoundException expected) {
            check(true, "an isolating URLClassLoader does not see the application loader's class");
        }
        try {
            Class<?> leaked = Class.forName(name, false, iso);
            throw new AssertionError("RLoaderChurnDefine: Class.forName(" + name
                    + ", false, isolatingLoader) must raise ClassNotFoundException; got " + leaked
                    + " owned by " + leaked.getClassLoader());
        } catch (ClassNotFoundException expected) {
            check(true, "...and Class.forName through the same loader agrees");
        }

        // Isolated, not broken: bootstrap delegation is untouched.
        try {
            Class<?> boot = iso.loadClass("java.util.zip.CRC32");
            check(boot.getClassLoader() == null,
                    "the isolating loader still delegates to the bootstrap loader");
        } catch (ClassNotFoundException e) {
            throw new AssertionError(
                    "RLoaderChurnDefine: the isolating loader lost bootstrap delegation: " + e);
        }

        // The discriminator, inverted into an invariant.
        java.net.URLClassLoader isoSub = new SubUrlLoader(urls, null);
        try {
            Class<?> leaked = isoSub.loadClass(name);
            throw new AssertionError("RLoaderChurnDefine: a URLClassLoader SUBCLASS must answer "
                    + "identically to a bare instance; it returned " + leaked);
        } catch (ClassNotFoundException expected) {
            check(true, "a URLClassLoader SUBCLASS answers identically -- the asymmetry is gone");
        }

        // The over-correction guard: parent-first delegation still works.
        java.net.URLClassLoader delegating = new java.net.URLClassLoader(urls, app);
        try {
            check(delegating.loadClass(name) == Sealed.class,
                    "a bare URLClassLoader parented to the application loader still delegates "
                            + "to it, and answers with the PARENT'S class object");
        } catch (ClassNotFoundException e) {
            throw new AssertionError("RLoaderChurnDefine: over-correction -- parent-first "
                    + "delegation through a bare URLClassLoader broke: " + e);
        }
        try {
            check("sealed".equals(Sealed.class.getMethod("tag").invoke(null)),
                    "the application loader's copy still runs");
        } catch (ReflectiveOperationException e) {
            throw new AssertionError("RLoaderChurnDefine: Sealed.tag() failed: " + e);
        }
        System.out.println("CK RLoaderChurnDefine isolatingLoader=ok");
    }

    /**
     * A parent loader whose {@code loadClass(String,boolean)} raises something
     * other than {@code ClassNotFoundException}.
     *
     * {@code loadClass(String,boolean)} is the canonical override point — it is
     * the one Spring's {@code OverridingClassLoader}, Tomcat's
     * {@code WebappClassLoaderBase} and every OSGi-shaped loader override — and
     * it is also the form both VMs reach: HotSpot's
     * {@code ClassLoader.loadClass} calls {@code parent.loadClass(name, false)}
     * directly, and CratonVM's parent-delegation native calls the one-argument
     * form, which routes into this override through
     * {@code receiver_overrides_load_class_resolve}. Overriding the two-argument
     * form therefore exercises the same body on both.
     */
    static final class HostileParent extends ClassLoader {
        /** Raises an unchecked exception — NOT a {@code ClassNotFoundException}. */
        static final String BOOM = "no.such.pkg.ParentBoom";
        /** Raises the exception the JDK's own {@code catch} names. */
        static final String ABSENT = "RLoaderChurnDefine$Echo";

        int boomCalls;
        int absentCalls;

        HostileParent(ClassLoader parent) {
            super(parent);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (BOOM.equals(name)) {
                boomCalls++;
                throw new IllegalStateException("parent-boom");
            }
            if (ABSENT.equals(name)) {
                absentCalls++;
                throw new ClassNotFoundException(name);
            }
            return super.loadClass(name, resolve);
        }
    }

    /**
     * W7-26 R1 — a parent loader's FAILURE is not the same fact as its MISS.
     *
     * JDK 25 {@code ClassLoader.loadClass(String,boolean)} delegates inside
     * exactly one {@code catch}:
     *
     * <pre>
     *   try { c = parent.loadClass(name, false); }
     *   catch (ClassNotFoundException e) { }
     * </pre>
     *
     * — so a {@code ClassNotFoundException} from the parent means "keep going"
     * and everything else leaves {@code loadClass}. CratonVM's real-mode
     * parent-delegation native matched the first half of that on a bare
     * {@code _ =>} arm which also caught the second: an
     * {@code IllegalStateException}, a {@code LinkageError}, or an
     * {@code ExceptionInInitializerError} out of the parent's own loader code
     * was recorded as "the parent does not have this class", and the caller
     * received a {@code ClassNotFoundException} naming the class instead. The
     * exception's identity was gone and the real cause was unnameable from
     * Java — a diagnosable failure turned into a wrong answer.
     *
     * ANTI-VACUITY, and both directions of it. Asserting only that something
     * throws would pass on the old behaviour, which threw too — just the wrong
     * class — so the FIRST check asserts the exception's TYPE and explicitly
     * fails on {@code ClassNotFoundException}. The over-correction is the
     * mirror image and is checked on the same call sites: a parent raising the
     * exception the JDK's {@code catch} DOES name must still be absorbed, the
     * child must still reach its own URL search afterwards, and a parent that
     * simply answers must still be the answer. A fix that propagated everything
     * would fail all three.
     *
     * The parent counts its own calls, so a run in which delegation never
     * happened at all cannot read green.
     */
    static void aParentsFailureIsNotAMiss() {
        String cp = System.getProperty("java.class.path");
        check(cp != null && !cp.isEmpty(),
                "java.class.path must be set, or this section tests nothing");
        String[] entries = cp.split(java.io.File.pathSeparator);
        java.net.URL[] urls = new java.net.URL[entries.length];
        for (int i = 0; i < entries.length; i++) {
            try {
                urls[i] = new java.io.File(entries[i]).toURI().toURL();
            } catch (java.net.MalformedURLException e) {
                throw new AssertionError("RLoaderChurnDefine: bad classpath entry " + entries[i]);
            }
        }

        // The shape a real application builds: a bare java.net.URLClassLoader
        // over its own URLs, wrapping a custom parent. Spring Boot's
        // PropertiesLauncher.wrapWithCustomClassLoader is this exact topology,
        // and it is named in the delegation native's own comment as the reason
        // that branch exists.
        HostileParent parent = new HostileParent(RLoaderChurnDefine.class.getClassLoader());
        java.net.URLClassLoader child = new java.net.URLClassLoader(urls, parent);

        // (1) The repair. A non-ClassNotFoundException failure from the parent
        //     must arrive at the caller as ITSELF.
        try {
            Class<?> wrong = child.loadClass(HostileParent.BOOM);
            throw new AssertionError("RLoaderChurnDefine: loadClass(" + HostileParent.BOOM
                    + ") must not answer at all; got " + wrong);
        } catch (ClassNotFoundException e) {
            throw new AssertionError("RLoaderChurnDefine: the parent raised IllegalStateException "
                    + "and the child reported ClassNotFoundException. A parent's FAILURE was "
                    + "recorded as a MISS, so the cause is unnameable from Java. (W7-26 R1)");
        } catch (IllegalStateException expected) {
            check("parent-boom".equals(expected.getMessage()),
                    "the parent's own exception object must arrive, not a rebuilt one");
        }
        check(parent.boomCalls == 1,
                "the parent must have been consulted exactly once, or this check measured nothing");

        // (2) Over-correction guard, half one: the exception the JDK's catch
        //     names is still absorbed, and the child still reaches its own URL
        //     search afterwards and defines its own copy.
        Class<?> own;
        try {
            own = child.loadClass(HostileParent.ABSENT);
        } catch (ClassNotFoundException e) {
            throw new AssertionError("RLoaderChurnDefine: over-correction -- the parent raised "
                    + "ClassNotFoundException, which ClassLoader.loadClass absorbs, and the child "
                    + "failed instead of searching its own URLs: " + e);
        }
        check(parent.absentCalls == 1, "the parent was consulted for the absorbed case too");
        check(HostileParent.ABSENT.equals(own.getName()), "the child answered with the right name");
        check(own.getClassLoader() == child,
                "...from its OWN URL search, after absorbing the parent's ClassNotFoundException");

        // (3) Over-correction guard, half two: a parent that simply answers is
        //     still the answer, through the same child instance.
        try {
            Class<?> viaParent = child.loadClass("RLoaderChurnDefine$Sealed");
            check(viaParent == Sealed.class,
                    "a parent that resolves normally still supplies the class object");
        } catch (ClassNotFoundException e) {
            throw new AssertionError("RLoaderChurnDefine: over-correction -- ordinary parent-first "
                    + "delegation through a custom parent broke: " + e);
        }
        System.out.println("CK RLoaderChurnDefine parentFailure=ok");
    }

    public static void main(String[] args) {
        manyLoadersOneName();
        churnWithSeveralNames();
        aSurvivorKeepsItsClass();
        repeatLookupIsACacheHit();
        anIsolatingLoaderCannotSeeTheAppLoadersClass();
        aParentsFailureIsNotAMiss();
        System.out.println("CK RLoaderChurnDefine checks=" + checks);
        System.out.println("PASS RLoaderChurnDefine (" + checks + " checks)");
    }
}
