import java.io.File;
import java.net.URL;
import java.net.URLClassLoader;

/**
 * Correctness net for the per-thread virtual-dispatch-target memo.
 *
 * The memo records, per (call site, receiver ClassId), the receiver's class
 * name AND whether that name resolves back to exactly this ClassId
 * ("globally named"). The second half is what a *second class loader defining
 * the same binary name* invalidates — it redefines nothing, publishes no
 * compiled code and supersedes no tier, so it is invisible to every other
 * dispatch-cache flush signal. `class_definition_epoch` exists for it.
 *
 * The shape here is the one that actually occurs (Groovy scripts, Tomcat
 * webapps, WildFly modules): two sibling loaders each define their OWN class
 * under one name, and both instances reach the same interface call site.
 *
 * The first loader's class is driven hot ALONE first, so its memo entry is
 * populated and `globally_named` is true, before the colliding class exists.
 * If the epoch did not invalidate that entry, the second loader's instances
 * would be at risk of resolving their callee by name into the first loader's
 * copy.
 */
public final class LoaderNameCollisionDispatchProbe {

    public interface Task {
        String id();
        int weight();
    }

    private static Task make(String dir) throws Exception {
        URL url = new File(dir).toURI().toURL();
        // Parent is this probe's loader, which does NOT have Impl on its
        // classpath — so each child loader defines its own copy.
        URLClassLoader loader = new URLClassLoader(new URL[] { url },
                LoaderNameCollisionDispatchProbe.class.getClassLoader());
        Class<?> impl = loader.loadClass("Impl");
        return (Task) impl.getDeclaredConstructor().newInstance();
    }

    private static long drive(Task t, int iterations, String wantId, int wantWeight) {
        long acc = 0;
        for (int i = 0; i < iterations; i++) {
            if (!wantId.equals(t.id())) {
                throw new AssertionError("id() resolved into the wrong loader's class: got "
                        + t.id() + " want " + wantId + " at i=" + i);
            }
            int w = t.weight();
            if (w != wantWeight) {
                throw new AssertionError("weight() resolved into the wrong loader's class: got "
                        + w + " want " + wantWeight + " at i=" + i);
            }
            acc += w;
        }
        return acc;
    }

    public static void main(String[] args) throws Exception {
        String dirA = args.length > 0 ? args[0] : "collA";
        String dirB = args.length > 1 ? args[1] : "collB";
        int iterations = args.length > 2 ? Integer.parseInt(args[2]) : 200_000;

        Task a = make(dirA);
        if (!"A".equals(a.id())) {
            throw new AssertionError("fixture: expected loader A to define id()=A, got " + a.id());
        }

        // Phase 1 — drive A alone until the call site is hot and its memo entry
        // is populated, while "Impl" still resolves uniquely to A's copy.
        long accA = drive(a, iterations, "A", 1);

        // Phase 2 — NOW define the colliding class. Nothing is redefined and no
        // code is superseded; only the name->ClassId mapping changed.
        Task b = make(dirB);
        if (!"B".equals(b.id())) {
            throw new AssertionError("fixture: expected loader B to define id()=B, got " + b.id());
        }

        // Phase 3 — both instances through the same call site, interleaved.
        for (int round = 0; round < 4; round++) {
            accA += drive(a, iterations / 4, "A", 1);
            accA += drive(b, iterations / 4, "B", 2);
        }

        // Phase 4 — a third loader on the same name, after everything is hot.
        Task c = make(dirA);
        if (c.getClass() == a.getClass()) {
            throw new AssertionError("fixture: loader C should define its own copy");
        }
        accA += drive(c, iterations / 4, "A", 1);
        accA += drive(b, iterations / 4, "B", 2);
        accA += drive(a, iterations / 4, "A", 1);

        System.out.println("LOADER_NAME_COLLISION_DISPATCH_OK acc=" + accA);
    }
}
