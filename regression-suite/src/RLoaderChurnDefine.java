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

    public static void main(String[] args) {
        manyLoadersOneName();
        churnWithSeveralNames();
        aSurvivorKeepsItsClass();
        System.out.println("CK RLoaderChurnDefine checks=" + checks);
        System.out.println("PASS RLoaderChurnDefine (" + checks + " checks)");
    }
}
