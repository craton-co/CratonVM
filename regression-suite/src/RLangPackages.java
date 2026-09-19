import java.util.HashSet;
import java.util.Set;

/**
 * The ten registrations of {@code i2_register_classloader_package_natives}, and
 * the ONLY vector in this corpus that can price them.
 *
 * <p>{@code H25-3} R4 refused to adjudicate that registrar for a reason that was
 * a property of the corpus rather than of the code: a {@code grep} over all 107
 * vectors found <b>no call to {@code getDefinedPackage}, {@code getPackages()}
 * or {@code getPackage()} anywhere</b>, and the only mentions of the frameworks
 * the registrar's comments name — Mockito, Byte Buddy, WildFly, Spring — were
 * class-name string literals. Arming {@code java/lang/ClassLoader} +
 * {@code java/lang/Package} would therefore have reported zero failures, and
 * that zero would have carried no information whatever. {@code H25-3} N3 asked
 * for exactly this file: <i>"the corpus gains a vector that calls
 * getDefinedPackage/getPackages/Package.equals directly — cheap, they are three
 * lines each."</i>
 *
 * <p>Every row below is a VALUE assertion, not a presence assertion. That is the
 * distinction trap 5 of the worker briefs is about: a green arm is evidence
 * about the question it asked, and "the call returned an object" is not the
 * question. Each check names what it read.
 *
 * <p>The identity rows are the load-bearing ones. HotSpot interns one
 * {@code Package} per (loader, name); CratonVM allocated a FRESH synthetic
 * {@code Package} on every {@code Class.getPackage()}, so identity
 * {@code equals} was always false and Spring's
 * {@code MvcParamPredicate.hasMvcAnnotation} misclassified annotations by
 * declaring package. {@code Package.equals}/{@code hashCode} are registered to
 * repair that, and until now nothing checked whether they still do.
 */
public class RLangPackages {
    static int checks = 0;

    static void ck(String what, boolean ok, String detail) {
        checks++;
        if (!ok) {
            throw new AssertionError(what + ": " + detail);
        }
    }

    /**
     * Element-wise validation that does NOT touch {@code checks}.
     *
     * <p>The published check count has to name the same number of propositions
     * on CratonVM as on the HotSpot oracle, or the harness diff fires on the
     * count rather than on a defect. Package-array LENGTHS differ between the
     * two for a documented reason, so the elements are folded into one
     * proposition here instead of counted individually.
     */
    static boolean allNamedPackages(Package[] ps) {
        for (Package p : ps) {
            if (p == null || p.getName() == null) {
                return false;
            }
        }
        return true;
    }

    static void eq(String what, Object got, Object want) {
        checks++;
        if (want == null ? got != null : !want.equals(got)) {
            throw new AssertionError(what + ": expected [" + want + "] got [" + got + "]");
        }
    }

    public static void main(String[] args) throws Exception {
        Class<?> self = RLangPackages.class;
        ClassLoader appLoader = self.getClassLoader();

        // ---- Class.getPackage / getPackageName -------------------------------
        // This class is in the unnamed package. Since JDK 9 that package HAS a
        // Package object and its name is the empty string -- assert the NAME,
        // not the presence. (The first draft of this vector asserted
        // `getPackage() == null`, which is the pre-JDK-9 contract; the HotSpot
        // oracle rejected it before CratonVM was ever asked. A vector's own
        // premise is code that can be wrong.)
        eq("self.getPackageName", self.getPackageName(), "");
        Package unnamed = self.getPackage();
        ck("self.getPackage!=null", unnamed != null, "the unnamed package has a Package object");
        eq("self.getPackage.getName", unnamed.getName(), "");
        ck("unnamed package equals itself across two calls",
                unnamed.equals(self.getPackage()),
                "two getPackage() calls on one class disagreed");

        Package langPkg = String.class.getPackage();
        ck("String.getPackage!=null", langPkg != null, "java.lang.String has a package");
        eq("String.getPackage.getName", langPkg.getName(), "java.lang");
        eq("String.getPackageName", String.class.getPackageName(), "java.lang");
        // isSealed() is a property of how the image was BUILT, not of the
        // contract, so it is reported rather than asserted: the harness diffs
        // this line against HotSpot's, which is the assertion that can be
        // right without this file guessing the answer. (It guessed `false`
        // first, and Temurin 25 says `true`.)
        System.out.println("CK RLangPackages javaLangSealed=" + langPkg.isSealed());

        // ---- the identity contract -------------------------------------------
        // Two INDEPENDENT classes in java.lang must yield equal Package objects,
        // and equal ones must hash equal. A fresh synthetic Package per call
        // fails the first row; an equals() without a matching hashCode() fails
        // the second.
        Package viaInteger = Integer.class.getPackage();
        ck("Package.equals across two java.lang classes",
                langPkg.equals(viaInteger),
                "String.class.getPackage() != Integer.class.getPackage()");
        ck("Package.equals is symmetric", viaInteger.equals(langPkg), "asymmetric equals");
        ck("Package.hashCode agrees with equals",
                langPkg.hashCode() == viaInteger.hashCode(),
                "equal Packages hashed " + langPkg.hashCode() + " vs " + viaInteger.hashCode());
        ck("Package.equals(null) is false", !langPkg.equals(null), "equals(null) said true");
        ck("Package.equals(String) is false", !langPkg.equals("java.lang"),
                "equals of a foreign type said true");

        // A HashSet is the consumer that needs both halves at once. Spring's
        // annotation classification is this shape.
        Set<Package> set = new HashSet<>();
        set.add(langPkg);
        set.add(viaInteger);
        eq("HashSet<Package> dedups", set.size(), 1);
        ck("HashSet<Package> contains", set.contains(String.class.getPackage()),
                "a third getPackage() of the same package was not found in the set");

        // Different packages must NOT collapse.
        Package utilPkg = java.util.ArrayList.class.getPackage();
        eq("java.util package name", utilPkg.getName(), "java.util");
        ck("java.lang != java.util", !langPkg.equals(utilPkg), "two packages compared equal");

        // ---- ClassLoader.getDefinedPackage / getDefinedPackages --------------
        Package defined = appLoader.getDefinedPackage("java.lang");
        ck("appLoader.getDefinedPackage(java.lang)==null", defined == null,
                "the application loader does not DEFINE java.lang, got " + defined);

        Package[] definedByApp = appLoader.getDefinedPackages();
        ck("getDefinedPackages() non-null", definedByApp != null, "returned null");
        // The array's runtime component type is asserted, not just its length:
        // a Value-typed reference array reports Object[] and every element read
        // through it is a silent widening. Nothing else in this corpus checks it
        // for this call.
        eq("getDefinedPackages component type",
                definedByApp.getClass().getComponentType(), Package.class);
        // ONE check for the whole array, not one per element: the count this
        // vector publishes must assert the same NUMBER of propositions on both
        // VMs, and the two legitimately define different numbers of packages.
        // See the note at the bottom of the file.
        ck("getDefinedPackages elements are non-null, named Packages",
                allNamedPackages(definedByApp), "an element was null or unnamed");

        // `java.lang` is DEFINED by the boot loader, which is `null` and has no
        // `getDefinedPackage` to call, so neither the application nor the
        // platform loader may claim it. Both must answer null; a loader that
        // fabricates a Package for a name it did not define is the failure this
        // pair is here for.
        ClassLoader platform = ClassLoader.getPlatformClassLoader();
        ck("platform.getDefinedPackage(java.lang)==null",
                platform.getDefinedPackage("java.lang") == null,
                "the platform loader does not define java.lang");
        ck("platform.getDefinedPackage(absent)==null",
                platform.getDefinedPackage("no.such.package.here") == null,
                "a package nobody defined was fabricated");
        ck("appLoader.getDefinedPackage(absent)==null",
                appLoader.getDefinedPackage("no.such.package.here") == null,
                "a package nobody defined was fabricated");

        // ---- Package.toString and the version accessors -----------------------
        // Values, not presence. An unversioned package must answer null for all
        // six, and toString must say so.
        // Same rule: the six version accessors read the image's manifest, so
        // their values are reported and diffed rather than pinned here.
        System.out.println("CK RLangPackages specTitle=" + langPkg.getSpecificationTitle()
                + " specVersion=" + langPkg.getSpecificationVersion()
                + " specVendor=" + langPkg.getSpecificationVendor());
        System.out.println("CK RLangPackages implTitle=" + langPkg.getImplementationTitle()
                + " implVersion=" + langPkg.getImplementationVersion()
                + " implVendor=" + langPkg.getImplementationVendor());
        ck("Package.toString names the package",
                langPkg.toString().contains("java.lang"),
                "toString was " + langPkg.toString());

        // ---- getPackages() ----------------------------------------------------
        // The WildFly row: getPackages()'s stream pipeline must produce a real
        // Package[], not a ReferencePipeline$Head typed as one. The component
        // type check is the assertion that separates them; `arraylength` on the
        // fabricated shape is where org.jboss.modules died.
        Package[] all = Package.getPackages();
        ck("Package.getPackages() non-null", all != null, "returned null");
        eq("Package.getPackages() component type", all.getClass().getComponentType(), Package.class);
        ck("getPackages elements are non-null, named Packages",
                allNamedPackages(all), "an element was null or unnamed");
        // WHAT THIS BLOCK DELIBERATELY DOES NOT ASSERT, and why.
        //
        // The CONTENTS of Package.getPackages() are a KNOWN DIVERGENCE, not an
        // untested property. CratonVM overrides ClassLoader.getPackages() to an
        // unconditionally EMPTY array on purpose: the real JDK bytecode is
        // `packages().toArray(Package[]::new)`, and in this VM's boot that
        // stream pipeline leaks a ReferencePipeline$Head into the caller's
        // `Package[]` local, NPE-ing on arraylength inside
        // org/jboss/modules/ConcurrentClassLoader.<clinit> (WildFly 39 boot).
        // The decision and its reason are recorded at the registration site in
        // native-builtins/src/lang_class.rs.
        //
        // So this vector asserts the SHAPE the override owes its caller either
        // way -- a Package[] and not an Object[] -- and leaves the count alone.
        // Asserting `length > 0` would be asking CratonVM to break WildFly;
        // asserting `length == 0` would freeze a divergence into the gate and
        // call it correct.

        System.out.println("PASS RLangPackages (" + checks + " checks)");
    }
}
