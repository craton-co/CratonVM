import java.lang.reflect.Field;
import java.lang.reflect.Modifier;
import java.sql.SQLException;
import java.util.concurrent.atomic.AtomicIntegerFieldUpdater;
import java.util.concurrent.atomic.AtomicLongFieldUpdater;
import java.util.concurrent.atomic.AtomicReferenceFieldUpdater;

/**
 * JDK-only corpus: {@code java.sql} must be loadable, and
 * {@code Atomic{Reference,Integer,Long}FieldUpdater} must be the JDK's own.
 *
 * <h2>The defect this pins</h2>
 *
 * {@code java.sql.SQLException} holds
 * {@code private static final AtomicReferenceFieldUpdater<SQLException,SQLException> nextUpdater},
 * so its {@code <clinit>} runs {@code newUpdater}. CratonVM registered a Rust
 * native for that factory which returned a fabricated
 * {@code ...FieldUpdater$RustJvmImpl} receiver -- a class no JDK image declares.
 * Under {@code --jdk-only} the VM correctly refuses to fabricate it, so the
 * refusal arrived at the application as
 *
 * <pre>
 * NoClassDefFoundError: java/util/concurrent/atomic/AtomicReferenceFieldUpdater$RustJvmImpl
 *   at java/sql/SQLException.&lt;clinit&gt;(SQLException.java:374)
 * </pre>
 *
 * and took the ENTIRE {@code java.sql} package with it: all JDBC, every ORM,
 * every pool. It also corrupts exception identity far from JDBC -- H2's
 * {@code DbException extends SQLException}, which is how
 * {@code org.h2.test.unit.TestStringUtils} reported "expected {@code DbException},
 * got {@code NoClassDefFoundError}". The refusal was right; the survival of its
 * caller was the defect. Fixed by not registering the module under
 * {@code CompatibilityMode::JdkOnly} (see the header of
 * {@code native-builtins/src/atomic_updater.rs}).
 *
 * <h2>Why the assertions are shaped the way they are</h2>
 *
 * <b>Non-null is not the contract.</b> Two defects survived this year behind a
 * {@code != null} and a length check, so nothing here is satisfied by an object
 * merely existing:
 *
 * <ul>
 *   <li><b>ONE storage location, proved in both directions.</b> A field updater
 *       and a plain {@code getfield} of the same field must address the same
 *       word. This is not pedantry: when CratonVM's {@code objectFieldOffset1}
 *       cannot resolve a (class, field) pair it MINTS a synthetic offset and
 *       routes {@code Unsafe} loads/stores through a side table
 *       ({@code SYNTHETIC_OFFSET_BASE}, {@code native-builtins/src/lib.rs}),
 *       which a direct field read cannot see. Under that failure the updater
 *       "works" perfectly against itself and {@code SQLException.getNextException()}
 *       -- a plain field read -- returns null forever. So every reference-updater
 *       write is read back through the Java field AND through
 *       {@code Field.get}, and every direct write is read back through the
 *       updater.</li>
 *   <li><b>Identity, not equality.</b> The reference values are
 *       {@code new String(...)}, so a check that compared with {@code equals}
 *       would pass where {@code ==} fails. Both are asserted where both are
 *       facts.</li>
 *   <li><b>The long variant carries a value above 2^32</b> and a negative one,
 *       so an implementation that truncates through {@code int} -- the shape
 *       {@code native_alfu_get}'s {@code Value::Int(i) => i as i64} arm would
 *       take -- cannot pass.</li>
 *   <li><b>The negatives assert the JDK's own exception TYPES</b>, printed by
 *       simple name so the cross-VM diff compares them rather than a boolean.
 *       Messages are never printed: they legitimately differ.</li>
 * </ul>
 *
 * <h2>Mode</h2>
 *
 * This is a {@code --jdk-only} POLICY vector and belongs to
 * {@code JDKONLY_CLASSES}. The {@code nonVolatileRejected()} arm asserts
 * HotSpot's {@code IllegalArgumentException("Must be volatile type")}, which the
 * Compatible-mode Rust native does NOT implement (it rejects static and final
 * but accepts a plain non-volatile field). That arm is therefore expected to be
 * RED without {@code --jdk-only}, and it is deliberately kept rather than
 * softened: it is the JDK contract, and the divergence is a real Compatible-mode
 * finding, reported rather than hidden. Run this vector with
 * {@code CRATONVM_ARGS=--jdk-only}.
 *
 * Determinism: single-threaded, no timing, no identity hash codes, no exception
 * messages printed.
 */
public class RJdkSqlPackage {
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError("RJdkSqlPackage: " + m);
        }
    }

    /** The updater target. Public throughout so no access rule is in play. */
    public static class Holder {
        public volatile String ref;
        public volatile int i;
        public volatile long l;
        /** Non-volatile and non-final -- the JDK rejects an updater on this. */
        public String plain;
        /** Non-volatile because it is final -- a second witness for the same rule. */
        public final String fin = "fin";
    }

    /** H2's {@code DbException} shape: an app exception under {@code java.sql}. */
    public static class AppSqlException extends SQLException {
        private static final long serialVersionUID = 1L;

        AppSqlException(String m) {
            super(m);
        }
    }

    // -----------------------------------------------------------------
    // A. java.sql loads, and it is the real class
    // -----------------------------------------------------------------

    static void sqlPackageLoads() throws Exception {
        Class<?> c = Class.forName("java.sql.SQLException");
        // Identity, not non-null: a fabricated stand-in would also be non-null.
        check(c == SQLException.class, "Class.forName must yield the very same Class object");
        check(c.getName().equals("java.sql.SQLException"), "name: " + c.getName());
        check(c.getSuperclass() == Exception.class,
                "SQLException must extend java.lang.Exception, got " + c.getSuperclass());
        check(Throwable.class.isAssignableFrom(c), "SQLException must be a Throwable");

        // The package, not just the one class: each of these has its own
        // <clinit> and its own reason to be reached by real JDBC.
        for (String n : new String[] {
                "java.sql.Connection", "java.sql.Statement", "java.sql.PreparedStatement",
                "java.sql.ResultSet", "java.sql.DriverManager", "java.sql.SQLTimeoutException",
                "java.sql.BatchUpdateException" }) {
            Class<?> k = Class.forName(n);
            check(k.getName().equals(n), "java.sql class name: " + k.getName());
        }
        // A subclass with a <clinit> of its own that runs SQLException's first.
        check(SQLException.class.isAssignableFrom(java.sql.SQLTimeoutException.class),
                "SQLTimeoutException must be a SQLException");
        System.out.println("CK RJdkSqlPackage sqlex=" + c.getName()
                + " super=" + c.getSuperclass().getName());
    }

    // -----------------------------------------------------------------
    // B. SQLException chaining -- the JDK's own nextUpdater.compareAndSet
    // -----------------------------------------------------------------

    static void sqlExceptionChains() {
        SQLException a = new SQLException("a", "S0001", 11);
        check(a.getMessage().equals("a"), "message: " + a.getMessage());
        check(a.getSQLState().equals("S0001"), "sqlstate: " + a.getSQLState());
        check(a.getErrorCode() == 11, "errorCode: " + a.getErrorCode());
        check(a.getNextException() == null, "a fresh SQLException has no next");

        SQLException b = new SQLException("b");
        a.setNextException(b);
        // setNextException goes through nextUpdater.compareAndSet, and
        // getNextException is a PLAIN FIELD READ of the same field. Identity
        // here is the one-storage-location property, on the JDK's own class.
        check(a.getNextException() == b, "a.next must BE b (identity)");
        check(b.getNextException() == null, "b has no next yet");

        SQLException d = new SQLException("d");
        a.setNextException(d);
        // The JDK walks to the END of the chain, so d lands behind b.
        check(a.getNextException() == b, "a.next must still BE b after appending d");
        check(b.getNextException() == d, "b.next must BE d (the walk reached the tail)");
        check(a.getNextException().getNextException() == d, "a -> b -> d");
        check(d.getNextException() == null, "d is the tail");

        StringBuilder chain = new StringBuilder();
        for (SQLException e = a; e != null; e = e.getNextException()) {
            chain.append(e.getMessage());
        }
        check(chain.toString().equals("abd"), "chain: " + chain);
        System.out.println("CK RJdkSqlPackage chain=" + chain + " state=" + a.getSQLState()
                + " code=" + a.getErrorCode());
    }

    /** Exception identity survives -- the TestStringUtils shape, without H2. */
    static void appSubclassIdentity() {
        Throwable caught = null;
        try {
            throw new AppSqlException("boom");
        } catch (SQLException e) {
            caught = e;
        }
        check(caught != null, "the app exception must be catchable as SQLException");
        check(caught.getClass() == AppSqlException.class,
                "caught as SQLException but the class must still be AppSqlException, got "
                        + caught.getClass().getName());
        check(caught instanceof SQLException, "instanceof SQLException");
        check(caught.getMessage().equals("boom"), "message: " + caught.getMessage());
        System.out.println("CK RJdkSqlPackage appsubclass=" + caught.getClass().getSimpleName());
    }

    // -----------------------------------------------------------------
    // C. The reference updater -- a real round trip, one storage location
    // -----------------------------------------------------------------

    static void referenceUpdater() throws Exception {
        Field f = Holder.class.getDeclaredField("ref");
        check(f.getType() == String.class, "ref must be declared String, got " + f.getType());
        // The JDK's newUpdater gates on exactly this bit. If CratonVM lost it,
        // the JDK path would reject a perfectly good field and this check names
        // the reason instead of leaving an IllegalArgumentException unexplained.
        check(Modifier.isVolatile(f.getModifiers()), "Holder.ref must report ACC_VOLATILE");

        AtomicReferenceFieldUpdater<Holder, String> u =
                AtomicReferenceFieldUpdater.newUpdater(Holder.class, String.class, "ref");

        Holder h = new Holder();
        String s1 = new String("s1");
        String s2 = new String("s2");
        String s3 = new String("s3");
        String s4 = new String("s4");

        check(u.get(h) == null, "a fresh holder reads null through the updater");

        // Direction 1: a DIRECT write must be visible to the updater.
        h.ref = s1;
        check(u.get(h) == s1, "updater.get must see the direct write (identity)");
        check(u.get(h).equals("s1"), "updater.get equality");

        // Direction 2: an UPDATER write must be visible to getfield AND to
        // core reflection. This is what a synthetic-offset side table fails.
        u.set(h, s2);
        check(h.ref == s2, "the Java field must see updater.set (identity)");
        check(f.get(h) == s2, "Field.get must see updater.set (identity)");

        // A failing CAS must change nothing and must say so.
        check(!u.compareAndSet(h, s1, s3), "CAS with a stale expected value must FAIL");
        check(h.ref == s2, "a failed CAS must leave the field alone");

        check(u.compareAndSet(h, s2, s3), "CAS with the current value must succeed");
        check(h.ref == s3, "a successful CAS must publish to the Java field");
        check(u.get(h) == s3, "and the updater must read it back");

        check(u.getAndSet(h, s4) == s3, "getAndSet returns the PREVIOUS value (identity)");
        check(h.ref == s4, "getAndSet publishes the new value");

        check(u.compareAndSet(h, s4, null), "CAS to null");
        check(h.ref == null, "the field is null");
        check(u.get(h) == null, "and reads null");
        check(f.get(h) == null, "and reflects null");

        // Two holders must not alias: a per-class offset used as if it were
        // per-object would tie these together.
        Holder h2 = new Holder();
        u.set(h, s1);
        u.set(h2, s2);
        check(h.ref == s1 && h2.ref == s2, "two holders must not alias");
        check(u.get(h) == s1 && u.get(h2) == s2, "and the updater must not alias them either");

        System.out.println("CK RJdkSqlPackage arfu h=" + h.ref + " h2=" + h2.ref
                + " reflect=" + f.get(h));
    }

    // -----------------------------------------------------------------
    // D. The int and long updaters
    // -----------------------------------------------------------------

    static void intUpdater() throws Exception {
        Field f = Holder.class.getDeclaredField("i");
        check(Modifier.isVolatile(f.getModifiers()), "Holder.i must report ACC_VOLATILE");
        AtomicIntegerFieldUpdater<Holder> u =
                AtomicIntegerFieldUpdater.newUpdater(Holder.class, "i");
        Holder h = new Holder();

        h.i = 7;
        check(u.get(h) == 7, "updater.get must see the direct write: " + u.get(h));
        u.set(h, 9);
        check(h.i == 9, "the Java field must see updater.set: " + h.i);
        check(((Integer) f.get(h)) == 9, "Field.get must see updater.set: " + f.get(h));

        check(!u.compareAndSet(h, 8, 10), "stale CAS must fail");
        check(h.i == 9, "a failed CAS changes nothing: " + h.i);
        check(u.compareAndSet(h, 9, 10), "current CAS must succeed");
        check(h.i == 10, "field after CAS: " + h.i);

        check(u.getAndAdd(h, 5) == 10, "getAndAdd returns the OLD value");
        check(h.i == 15, "field after getAndAdd: " + h.i);
        check(u.incrementAndGet(h) == 16, "incrementAndGet returns the NEW value");
        check(u.getAndIncrement(h) == 16, "getAndIncrement returns the OLD value");
        check(h.i == 17, "field after getAndIncrement: " + h.i);
        check(u.addAndGet(h, -7) == 10, "addAndGet returns the NEW value");
        check(u.decrementAndGet(h) == 9, "decrementAndGet returns the NEW value");
        check(u.getAndDecrement(h) == 9, "getAndDecrement returns the OLD value");
        check(h.i == 8, "field after getAndDecrement: " + h.i);
        check(u.getAndSet(h, 3) == 8, "getAndSet returns the OLD value");
        check(h.i == 3, "field after getAndSet: " + h.i);

        System.out.println("CK RJdkSqlPackage aifu i=" + h.i + " reflect=" + f.get(h));
    }

    static void longUpdater() throws Exception {
        Field f = Holder.class.getDeclaredField("l");
        check(Modifier.isVolatile(f.getModifiers()), "Holder.l must report ACC_VOLATILE");
        AtomicLongFieldUpdater<Holder> u = AtomicLongFieldUpdater.newUpdater(Holder.class, "l");
        Holder h = new Holder();

        // Above 2^32 on purpose: an implementation that routes through int
        // truncates this and cannot pass.
        final long big = 0x0000_0001_0000_0007L;
        final long neg = -0x0000_0002_0000_0001L;

        h.l = big;
        check(u.get(h) == big, "updater.get must see the direct write: " + u.get(h));
        u.set(h, neg);
        check(h.l == neg, "the Java field must see updater.set: " + h.l);
        check(((Long) f.get(h)) == neg, "Field.get must see updater.set: " + f.get(h));

        check(!u.compareAndSet(h, big, 0L), "stale CAS must fail");
        check(h.l == neg, "a failed CAS changes nothing: " + h.l);
        check(u.compareAndSet(h, neg, big), "current CAS must succeed");
        check(h.l == big, "field after CAS: " + h.l);

        check(u.getAndAdd(h, 1L) == big, "getAndAdd returns the OLD value");
        check(h.l == big + 1L, "field after getAndAdd: " + h.l);
        check(u.incrementAndGet(h) == big + 2L, "incrementAndGet returns the NEW value");
        check(u.addAndGet(h, -2L) == big, "addAndGet returns the NEW value");
        check(u.getAndSet(h, neg) == big, "getAndSet returns the OLD value");
        check(h.l == neg, "field after getAndSet: " + h.l);
        check(u.decrementAndGet(h) == neg - 1L, "decrementAndGet returns the NEW value");

        System.out.println("CK RJdkSqlPackage alfu l=" + h.l + " reflect=" + f.get(h));
    }

    // -----------------------------------------------------------------
    // E. The negatives -- the JDK's own exception types
    // -----------------------------------------------------------------

    /** A non-volatile, non-final field: {@code IllegalArgumentException}. */
    static void nonVolatileRejected() {
        String got = "<none>";
        try {
            AtomicReferenceFieldUpdater.newUpdater(Holder.class, String.class, "plain");
        } catch (Throwable t) {
            got = t.getClass().getSimpleName();
        }
        check(got.equals("IllegalArgumentException"),
                "newUpdater on a non-volatile field must throw IllegalArgumentException, got " + got);
        System.out.println("CK RJdkSqlPackage reject-nonvolatile=" + got);
    }

    /** A final (therefore non-volatile) field: {@code IllegalArgumentException}. */
    static void finalRejected() {
        String got = "<none>";
        try {
            AtomicReferenceFieldUpdater.newUpdater(Holder.class, String.class, "fin");
        } catch (Throwable t) {
            got = t.getClass().getSimpleName();
        }
        check(got.equals("IllegalArgumentException"),
                "newUpdater on a final field must throw IllegalArgumentException, got " + got);
        System.out.println("CK RJdkSqlPackage reject-final=" + got);
    }

    /** {@code vclass} that is not the declared field type: {@code ClassCastException}. */
    static void wrongTypeRejected() {
        String got = "<none>";
        try {
            AtomicReferenceFieldUpdater.newUpdater(Holder.class, Integer.class, "ref");
        } catch (Throwable t) {
            got = t.getClass().getSimpleName();
        }
        check(got.equals("ClassCastException"),
                "newUpdater with the wrong vclass must throw ClassCastException, got " + got);

        // The int updater on a reference field, and the long updater on an int
        // field: both are "Must be integer type" / "Must be long type" on
        // HotSpot, i.e. IllegalArgumentException.
        String gotInt = "<none>";
        try {
            AtomicIntegerFieldUpdater.newUpdater(Holder.class, "ref");
        } catch (Throwable t) {
            gotInt = t.getClass().getSimpleName();
        }
        check(gotInt.equals("IllegalArgumentException"),
                "AtomicIntegerFieldUpdater on a String field: " + gotInt);

        String gotLong = "<none>";
        try {
            AtomicLongFieldUpdater.newUpdater(Holder.class, "i");
        } catch (Throwable t) {
            gotLong = t.getClass().getSimpleName();
        }
        check(gotLong.equals("IllegalArgumentException"),
                "AtomicLongFieldUpdater on an int field: " + gotLong);

        System.out.println("CK RJdkSqlPackage reject-type=" + got + "," + gotInt + "," + gotLong);
    }

    public static void main(String[] args) throws Exception {
        sqlPackageLoads();
        sqlExceptionChains();
        appSubclassIdentity();
        referenceUpdater();
        intUpdater();
        longUpdater();
        nonVolatileRejected();
        finalRejected();
        wrongTypeRejected();
        System.out.println("CK RJdkSqlPackage checks=" + checks);
        System.out.println("PASS RJdkSqlPackage (" + checks + " checks)");
    }
}
