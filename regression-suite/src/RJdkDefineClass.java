import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.net.URL;
import java.nio.ByteBuffer;
import java.security.CodeSource;
import java.security.ProtectionDomain;
import java.security.cert.Certificate;

/**
 * JDK-only corpus: {@code ClassLoader.defineClass1} / {@code defineClass2}
 * decode fidelity.
 *
 * These two are {@code ACC_NATIVE} in the real JDK, so the natives registered
 * over them ({@code lang_system::native_classloader_define_class0/1/2},
 * registered {@code Bridge} on the ESSENTIAL path and therefore live in every
 * mode) shadow nothing — the native IS the implementation. The risk is not
 * shadowing, it is decode fidelity, and the shape that has actually broken is
 * {@code defineClass2}'s heap-{@code ByteBuffer} argument:
 *
 *   * a heap buffer's bytes live on {@code hb}, not at a native address, and
 *     reading {@code address} (slot 0 of {@code Buffer}) as a pointer segfaults
 *     or raises "direct ByteBuffer has no native address";
 *   * a SLICED heap buffer additionally carries a non-zero {@code offset}, so
 *     a decoder that reads {@code hb} but ignores {@code offset} hands the
 *     parser bytes that start eight bytes early.
 *
 * The class defined here is a minimal but genuinely valid class file, so a
 * wrong offset is a {@code ClassFormatError} rather than a silent pass, and the
 * name/superclass/loader are read back from the DEFINED class rather than from
 * the bytes, so a decoder that returns a plausible-looking wrong class fails.
 *
 * Determinism: no identity hashes; the code source URL is a fixed literal.
 */
public class RJdkDefineClass {
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError("RJdkDefineClass: " + m);
        }
    }

    /**
     * A valid, field-less, method-less {@code public final class <name>
     * extends Object}. Constant pool: #1 this-class, #2 name utf8, #3
     * super-class, #4 "java/lang/Object" utf8.
     */
    static byte[] tiny(String name) {
        ByteArrayOutputStream b = new ByteArrayOutputStream();
        DataOutputStream d = new DataOutputStream(b);
        try {
            d.writeInt(0xCAFEBABE);
            d.writeShort(0);            // minor
            d.writeShort(52);           // major (Java 8 — no stack maps needed)
            d.writeShort(5);            // constant_pool_count = 4 entries + 1
            d.writeByte(7); d.writeShort(2);          // #1 Class -> #2
            d.writeByte(1); d.writeUTF(name);         // #2 Utf8
            d.writeByte(7); d.writeShort(4);          // #3 Class -> #4
            d.writeByte(1); d.writeUTF("java/lang/Object"); // #4 Utf8
            d.writeShort(0x0031);       // ACC_PUBLIC|ACC_FINAL|ACC_SUPER
            d.writeShort(1);            // this_class = #1
            d.writeShort(3);            // super_class = #3
            d.writeShort(0);            // interfaces
            d.writeShort(0);            // fields
            d.writeShort(0);            // methods
            d.writeShort(0);            // attributes
        } catch (Exception e) {
            throw new RuntimeException(e);
        }
        return b.toByteArray();
    }

    static URL url() {
        try {
            return java.net.URI.create("file:/rjdkdefine").toURL();
        } catch (Exception e) {
            throw new RuntimeException(e);
        }
    }

    static class L extends ClassLoader {
        L() {
            super(null);
        }

        /** defineClass1 — the byte[] + offset + length form. */
        Class<?> viaArray(String n) {
            byte[] b = tiny(n);
            return defineClass(n, b, 0, b.length);
        }

        /** defineClass1 with a NON-ZERO array offset. */
        Class<?> viaOffsetArray(String n) {
            byte[] b = tiny(n);
            byte[] padded = new byte[b.length + 8];
            System.arraycopy(b, 0, padded, 8, b.length);
            return defineClass(n, padded, 8, b.length);
        }

        /** defineClass2 — the ByteBuffer form, zero-offset heap buffer. */
        Class<?> viaBuffer(String n) {
            byte[] b = tiny(n);
            ProtectionDomain pd = new ProtectionDomain(
                    new CodeSource(url(), (Certificate[]) null), null);
            return defineClass(n, ByteBuffer.wrap(b), pd);
        }

        /** defineClass2 with a heap buffer whose {@code offset} is non-zero. */
        Class<?> viaSlicedBuffer(String n) {
            byte[] b = tiny(n);
            byte[] padded = new byte[b.length + 8];
            System.arraycopy(b, 0, padded, 8, b.length);
            ByteBuffer bb = ByteBuffer.wrap(padded);
            bb.position(8);
            return defineClass(n, bb.slice(), null);
        }

        /** defineClass2 over a heap buffer whose position/limit window is the class. */
        Class<?> viaWindowedBuffer(String n) {
            byte[] b = tiny(n);
            byte[] padded = new byte[b.length + 12];
            System.arraycopy(b, 0, padded, 4, b.length);
            ByteBuffer bb = ByteBuffer.wrap(padded);
            bb.position(4).limit(4 + b.length);
            return defineClass(n, bb, null);
        }

        /** defineClass2 over a DIRECT buffer — the arm that really does have an address. */
        Class<?> viaDirectBuffer(String n) {
            byte[] b = tiny(n);
            ByteBuffer bb = ByteBuffer.allocateDirect(b.length);
            bb.put(b);
            bb.flip();
            return defineClass(n, bb, null);
        }

        /**
         * defineClass2 over bytes that stop three short of a class file.
         * {@code defineClass} is {@code protected}, so a caller outside this
         * subclass cannot reach it — this is the way in.
         */
        Class<?> viaTruncatedBuffer(String n) {
            byte[] t = tiny(n);
            return defineClass(n, ByteBuffer.wrap(t, 0, t.length - 3).slice(), null);
        }
    }

    static void defined(Class<?> c, String name, ClassLoader owner, String where) {
        check(c.getName().equals(name), where + ": name=" + c.getName() + " want " + name);
        check(c.getSuperclass() == Object.class, where + ": super=" + c.getSuperclass());
        check(c.getClassLoader() == owner, where + ": loader is the defining loader");
        check(c.getDeclaredMethods().length == 0, where + ": no declared methods");
        check(c.getDeclaredFields().length == 0, where + ": no declared fields");
        check(!c.isInterface() && !c.isArray() && !c.isPrimitive(), where + ": a plain class");
    }

    public static void main(String[] args) throws Exception {
        L l = new L();

        defined(l.viaArray("Zz1"), "Zz1", l, "defineClass1(byte[],0,len)");
        defined(l.viaOffsetArray("Zz2"), "Zz2", l, "defineClass1(byte[],8,len)");

        Class<?> c3 = l.viaBuffer("Zz3");
        defined(c3, "Zz3", l, "defineClass2(heap buffer)");
        check(c3.getProtectionDomain().getCodeSource().getLocation().toString()
                .equals("file:/rjdkdefine"), "the ProtectionDomain argument reached the class");

        defined(l.viaSlicedBuffer("Zz4"), "Zz4", l, "defineClass2(sliced heap buffer)");
        defined(l.viaWindowedBuffer("Zz5"), "Zz5", l, "defineClass2(windowed heap buffer)");
        defined(l.viaDirectBuffer("Zz6"), "Zz6", l, "defineClass2(direct buffer)");

        // Two loaders may define the same name; the classes are distinct.
        L other = new L();
        Class<?> a = l.viaArray("Zz7");
        Class<?> b = other.viaArray("Zz7");
        check(a != b, "two loaders define two distinct Zz7 classes");
        check(a.getName().equals(b.getName()), "…with the same name");

        // JVMS 5.3.5: one loader may not define the same name twice. HotSpot
        // raises `java.lang.LinkageError` ITSELF (not a subclass), with the
        // message "loader <L> attempted duplicate class definition for Zz7." —
        // the parenthetical tail HotSpot appends carries an identity hash, so
        // only the type and the stable phrase are asserted.
        //
        // This was NOT ASSERTED until 2026-08-11: CratonVM served the
        // already-defined mirror for every "already defined" backend error,
        // because that tolerance covers a DIFFERENT shape — two distinct
        // loaders colliding inside one of CratonVM's flat namespaces, which is
        // what a Tomcat webapp stop/start loop produces by its ~14th
        // WebappClassLoader. The two are now told apart by defining-loader
        // OBJECT identity; the case below is the same object, twice.
        boolean dup = false;
        String dupMessage = null;
        try {
            l.viaArray("Zz7");
        } catch (LinkageError e) {
            dup = true;
            dupMessage = String.valueOf(e.getMessage());
            check(e.getClass() == LinkageError.class,
                    "a duplicate definition raises LinkageError itself, not "
                            + e.getClass().getName());
        }
        check(dup, "a duplicate definition in one loader raises LinkageError");
        check(dupMessage != null && dupMessage.contains("attempted duplicate class definition")
                        && dupMessage.contains("Zz7"),
                "the LinkageError names the offence and the class: " + dupMessage);

        // …and it does NOT depend on which defineClass overload was used first:
        // the array form defined Zz7, the ByteBuffer form must refuse it too.
        boolean dupAcrossOverloads = false;
        try {
            l.viaBuffer("Zz7");
        } catch (LinkageError e) {
            dupAcrossOverloads = true;
        }
        check(dupAcrossOverloads,
                "defineClass2 refuses a name defineClass1 already defined in this loader");

        // The CONTROL, and the half that must not regress: a DIFFERENT loader
        // defining the same name is legal on HotSpot and stays legal here.
        // `other` already defined its own Zz7 above; define one more name in
        // both loaders to show the rule is about the loader, not the name.
        L third = new L();
        defined(third.viaArray("Zz7"), "Zz7", third, "a third loader defines Zz7 too");
        check(third.viaArray("Zz9") != l.viaArray("Zz9"),
                "two loaders defining the same fresh name get two distinct classes");

        // Truncated bytes are a ClassFormatError, not a crash and not a stub.
        boolean malformed = false;
        try {
            l.viaTruncatedBuffer("Zz8");
        } catch (ClassFormatError e) {
            malformed = true;
        }
        check(malformed, "a truncated ByteBuffer raises ClassFormatError");

        System.out.println("CK RJdkDefineClass checks=" + checks);
        System.out.println("PASS RJdkDefineClass (" + checks + " checks)");
    }
}
