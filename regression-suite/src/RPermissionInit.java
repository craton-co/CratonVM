import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.ObjectInputStream;
import java.io.ObjectOutputStream;
import java.security.Permission;
import java.security.PermissionCollection;
import java.security.Permissions;
import java.util.PropertyPermission;

/**
 * Regression: the permission family's constructors ran a native that wrote
 * {@code name} and returned, so every field their real bodies initialise stayed
 * at its default.
 *
 * WHY THIS VECTOR EXISTS. {@code b448f2039} FABRICATED
 * {@code java.security.Permission} and four subclasses — {@code
 * synthetic_stub_fields} gave them one field and {@code synthetic_stub_methods}
 * gave them their constructors — so a closure that wrote {@code name} really was
 * the whole implementation. The real classes are loaded now. The closure was
 * still winning, so {@code BasicPermission.<init>}'s {@code init(name)} and
 * {@code PropertyPermission.<init>}'s {@code init(getMask(actions))} never ran:
 *
 * <pre>
 *   new PropertyPermission("a.b.*", "read,write")
 *     HotSpot   mask=3  path="a.b.*"  getActions()="read,write"
 *     CratonVM  mask=0  path=null     getActions()=""
 * </pre>
 *
 * Neither {@code init} nor {@code getMask} was broken — invoked reflectively
 * both answered exactly as HotSpot does. Only the constructors that should call
 * them were being replaced, which is why every assertion below goes through a
 * CONSTRUCTOR and then asks about BEHAVIOUR.
 *
 * WHAT IS ASSERTED, and why not just "did it throw". The broken VM constructed
 * these permissions happily; it built objects that were quietly inert. A
 * {@code getActions()} that answers "" and an {@code implies()} that returns
 * false are not exceptions, they are wrong answers — and for a permission class
 * the wrong answer is the dangerous direction. So: the canonical actions STRING,
 * the {@code implies} matrix in both directions, and the refusals the JDK owes
 * on malformed input.
 *
 * Found by a serialization round trip, which is the symptom furthest from the
 * cause: {@code readObject} calls {@code init(getMask(actions))} itself, and
 * threw {@code IllegalArgumentException: invalid actions mask} because
 * {@code actions} had been serialized as "".
 *
 * ORACLE: HotSpot 25, same assertions.
 */
public class RPermissionInit {
    static int checks = 0;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    static void mustThrow(String what, Runnable r) {
        checks++;
        try {
            r.run();
            throw new AssertionError(what + ": expected IllegalArgumentException, got none");
        } catch (IllegalArgumentException expected) {
            // correct
        }
    }

    static Object roundTrip(Object o) throws Exception {
        ByteArrayOutputStream b = new ByteArrayOutputStream();
        try (ObjectOutputStream os = new ObjectOutputStream(b)) {
            os.writeObject(o);
        }
        try (ObjectInputStream is = new ObjectInputStream(new ByteArrayInputStream(b.toByteArray()))) {
            return is.readObject();
        }
    }

    public static void main(String[] args) throws Exception {
        // ---- 1. the constructor must run the real init: actions are parsed and
        //         canonicalised. "" is what the broken VM answered for all of these.
        check("read".equals(new PropertyPermission("k", "read").getActions()),
                "read -> " + new PropertyPermission("k", "read").getActions());
        check("write".equals(new PropertyPermission("k", "write").getActions()),
                "write -> " + new PropertyPermission("k", "write").getActions());
        check("read,write".equals(new PropertyPermission("k", "read,write").getActions()),
                "read,write -> " + new PropertyPermission("k", "read,write").getActions());
        // canonical ORDER, not the input order, and case-insensitive parsing
        check("read,write".equals(new PropertyPermission("k", "write,read").getActions()),
                "write,read must canonicalise to read,write");
        check("read".equals(new PropertyPermission("k", "READ").getActions()),
                "READ must parse case-insensitively");
        check("read".equals(new PropertyPermission("k", " read ").getActions()),
                "surrounding whitespace must be tolerated");

        // ---- 2. implies, both directions. A null `path` made this NPE; a zero
        //         `mask` makes it answer false, which is the unsafe direction.
        PropertyPermission rw = new PropertyPermission("a.b.*", "read,write");
        check(rw.implies(new PropertyPermission("a.b.c", "read")), "rw implies a.b.c read");
        check(rw.implies(new PropertyPermission("a.b.c", "write")), "rw implies a.b.c write");
        check(rw.implies(new PropertyPermission("a.b.c", "read,write")), "rw implies a.b.c read,write");
        check(!rw.implies(new PropertyPermission("x.y", "read")), "rw must NOT imply x.y");
        PropertyPermission ro = new PropertyPermission("a.b.*", "read");
        check(!ro.implies(new PropertyPermission("a.b.c", "write")),
                "read-only must NOT imply write");
        check(new PropertyPermission("*", "read").implies(new PropertyPermission("any.thing", "read")),
                "the all-wildcard must imply any name");

        // ---- 3. BasicPermission's own init: `path` and the wildcard flag.
        RuntimePermission exit = new RuntimePermission("exitVM");
        check(exit.implies(new RuntimePermission("exitVM")), "RuntimePermission implies itself");
        check(new RuntimePermission("a.*").implies(new RuntimePermission("a.b")),
                "wildcard RuntimePermission implies a.b");
        check(!new RuntimePermission("a.*").implies(new RuntimePermission("b.c")),
                "a.* must NOT imply b.c");
        check("exitVM".equals(exit.getName()), "getName -> " + exit.getName());

        // ---- 4. the refusals the JDK owes. The broken VM accepted all of these.
        mustThrow("bogus actions", () -> new PropertyPermission("k", "bogus"));
        mustThrow("empty actions", () -> new PropertyPermission("k", ""));
        mustThrow("trailing junk", () -> new PropertyPermission("k", "readx"));
        mustThrow("empty name", () -> new RuntimePermission(""));

        // ---- 5. serialization, the symptom this was found by. `readObject`
        //         re-runs `init(getMask(actions))`, so an unparsed `actions`
        //         fails there rather than at construction.
        PropertyPermission back = (PropertyPermission) roundTrip(rw);
        check("read,write".equals(back.getActions()), "round-tripped actions -> " + back.getActions());
        check("a.b.*".equals(back.getName()), "round-tripped name -> " + back.getName());
        check(back.implies(new PropertyPermission("a.b.c", "write")), "round-tripped implies");

        PermissionCollection pc = rw.newPermissionCollection();
        pc.add(new PropertyPermission("a.b.*", "read,write"));
        PermissionCollection pcBack = (PermissionCollection) roundTrip(pc);
        check(pcBack.implies(new PropertyPermission("a.b.c", "read")),
                "round-tripped PropertyPermissionCollection implies");

        PermissionCollection bpc = exit.newPermissionCollection();
        bpc.add(new RuntimePermission("exitVM"));
        PermissionCollection bpcBack = (PermissionCollection) roundTrip(bpc);
        check(bpcBack.implies(new RuntimePermission("exitVM")),
                "round-tripped BasicPermissionCollection implies");

        Permissions ps = new Permissions();
        ps.add(new PropertyPermission("java.version", "read"));
        ps.add(new RuntimePermission("getClassLoader"));
        Permissions psBack = (Permissions) roundTrip(ps);
        check(psBack.implies(new PropertyPermission("java.version", "read")),
                "round-tripped Permissions implies the PropertyPermission");
        check(psBack.implies(new RuntimePermission("getClassLoader")),
                "round-tripped Permissions implies the RuntimePermission");
        check(!psBack.implies(new PropertyPermission("java.version", "write")),
                "round-tripped Permissions must NOT imply write");

        // ---- 6. equals/hashCode depend on the same parsed state.
        check(new PropertyPermission("k", "read,write").equals(new PropertyPermission("k", "write,read")),
                "equal permissions differing only in action order");
        check(new PropertyPermission("k", "read,write").hashCode()
                        == new PropertyPermission("k", "write,read").hashCode(),
                "equal permissions must share a hashCode");
        check(!new PropertyPermission("k", "read").equals(new PropertyPermission("k", "write")),
                "read must not equal write");

        // A permission whose actions never parsed compares equal to everything
        // with the same name, which is the shape the defect produced.
        Permission a = new PropertyPermission("same", "read");
        Permission b = new PropertyPermission("same", "write");
        check(!a.equals(b), "same name, different actions must not be equal");

        System.out.println("CK RPermissionInit checks=" + checks);
        System.out.println("PASS RPermissionInit (" + checks + " checks)");
    }
}
