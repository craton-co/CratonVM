import java.io.ByteArrayOutputStream;
import java.io.FilterInputStream;
import java.io.InputStream;
import java.io.InterruptedIOException;
import java.io.StreamTokenizer;
import java.io.StringReader;
import java.lang.reflect.Field;
import java.lang.reflect.Modifier;
import java.util.ArrayList;

/**
 * JDK-only corpus: the JPMS half of {@code Field.get}/{@code Field.set} and the
 * whole typed {@code getInt}/{@code setLong}/... family.
 *
 * <p>Every row here is a PAIR. {@code Reflection.verifyMemberAccess} asks two
 * questions that a field read must not confuse, and getting either one wrong
 * has a witness in the opposite direction:
 *
 * <ul>
 *   <li>The module question for a plain read is {@code exports}, NOT
 *       {@code opens}, and it is the same question for a public and a
 *       non-public field. Measured on Temurin 25.0.3: under
 *       {@code --add-opens java.base/java.lang=ALL-UNNAMED} a plain
 *       {@code Field.get} of {@code String.hash} STILL throws
 *       {@code IllegalAccessException} -- opens only unblocks
 *       {@code setAccessible}. Under
 *       {@code --add-exports java.base/jdk.internal.misc=ALL-UNNAMED} the
 *       public {@code Unsafe.INVALID_FIELD_OFFSET} reads fine while the private
 *       {@code Unsafe.theUnsafe} still throws.</li>
 *   <li>"A public field needs only exports" is not "a public field needs
 *       nothing": {@code Unsafe.INVALID_FIELD_OFFSET} is public static final on
 *       a public class, and it is unreadable because jdk.internal.misc is not
 *       exported. A VM that skips the check for public fields fabricates a
 *       success there -- hence every ALLOW row below is paired with a DENY row
 *       that differs only in the module edge or the modifiers.</li>
 *   <li>A public field of a NON-public class is unreachable from another
 *       package however exported that package is
 *       ({@code java.text.CalendarBuilder.WEEK_YEAR}).</li>
 *   <li>And the rule must not over-deny: a cross-module SUBCLASS reads its
 *       superclass's {@code protected} field with no {@code setAccessible} and
 *       no {@code --add-opens} at all ({@code ByteArrayOutputStream.buf}).</li>
 *   <li>...but that allowance is narrower than "a subclass may read it". JLS
 *       6.6.2.1 refines it by the RECEIVER: the read must go through an object
 *       whose class is the CALLER's own class or a subclass of it. Section 7
 *       asserts the whole matrix, and the row that decides whether an
 *       implementation understood the rule or merely pattern-matched it is the
 *       SIBLING: a different subclass of the same superclass is a subclass of
 *       the declaring class and is still refused, because it is not under the
 *       caller.</li>
 *   <li>The refinement is orthogonal to JPMS and it does not apply to
 *       {@code static} members. Measured on Temurin 25.0.3: neither
 *       {@code --add-exports java.base/java.io} nor
 *       {@code --add-opens java.base/java.io} moves ANY of the four receiver
 *       rows, and the same matrix on the {@code protected static}
 *       {@code PipedInputStream.PIPE_SIZE} answers OK for every receiver --
 *       the caller's own class, the declaring class, a sibling, an unrelated
 *       {@code Object}, and {@code null} (section 7b).</li>
 *   <li>A null receiver on an INSTANCE field is a {@code NullPointerException}
 *       and it OUTRANKS every access refusal, because
 *       {@code Field.checkAccess} dereferences
 *       {@code Modifier.isStatic(modifiers) ? null : obj.getClass()} while
 *       building its argument list, before {@code verifyMemberAccess} is
 *       entered. {@code String.hash.get(null)} answers
 *       {@code NullPointerException}, not {@code IllegalAccessException},
 *       though the field is denied on three separate grounds (section 4a).</li>
 * </ul>
 *
 * <p>Exception precedence, measured on Temurin 25.0.3 with every row carrying
 * TWO faults at once, because a row with one fault asserts nothing about
 * ordering. It is NOT a three-level ladder, and section 9 asserts all six
 * ranks: {@code IllegalAccessException} occurs at rank 2 AND at rank 5, and an
 * implementation that collapses those two fixes one pair while breaking
 * another.
 *
 * <pre>
 *   rank 1  NullPointerException      null receiver on an INSTANCE field
 *   rank 2  IllegalAccessException    access denied (JLS 6.6.1/6.6.2.1 + JPMS)
 *   rank 3  IllegalArgumentException  typed accessor vs field descriptor
 *   rank 4  IllegalArgumentException  receiver not an instance of the declarer
 *   rank 5  IllegalAccessException    final write
 *   rank 6  IllegalArgumentException  value type, on the generic set(Object)
 * </pre>
 *
 * <p>Ranks 1 and 2 are {@code Field.checkAccess} and the {@code setAccessible}
 * override skips BOTH, so the ordering is not even fixed within one program:
 * {@code String.hash.getBoolean(null)} is a {@code NullPointerException}
 * (rank 1 beats rank 3) while an overridden {@code ownPrivateInt.getBoolean(
 * null)} is an {@code IllegalArgumentException} (rank 3 goes first because
 * ranks 1-2 are gone). Section 9f asserts both. Ranks 1 and 4 do not exist for
 * a {@code static} field at all -- the receiver is ignored rather than
 * validated, and 9d asserts that too, so a check that fires on every non-null
 * receiver cannot pass.
 *
 * <p>DELIBERATELY NOT ASSERTED -- recorded here so they are not mistaken for
 * coverage. Both are measured on Temurin 25.0.3.
 *
 * <ul>
 *   <li>{@code Method.invoke} with a NULL receiver on an instance method is a
 *       {@code NullPointerException} -- rank 1 of the lattice above, reached
 *       the same way, by dereferencing {@code obj.getClass()} while
 *       {@code checkAccess}'s argument list is built. Section 10 asserts the
 *       receiver REFINEMENT on {@code Method.invoke} but not this row:
 *       CratonVM has no {@code null_receiver_on_instance_field} equivalent on
 *       the method path, and inventing one there was out of this lane's
 *       measured scope. The field half of the same rank is asserted in section
 *       4a.</li>
 *   <li>{@code Method.invoke} through an ARRAY receiver
 *       ({@code Object.clone} on an {@code int[]} from a non-subclass caller)
 *       is an {@code IllegalAccessException}. Not asserted because it turns on
 *       whether an implementation models an array class's superclass as
 *       {@code java.lang.Object}, which is a question about the class model and
 *       not about this rule. The FIELD path's array row IS asserted (9d,
 *       {@code bytesTransferred.get(new int[1])}), where a separate array-
 *       receiver rejection backstops it.</li>
 * </ul>
 *
 * <p>Determinism: no addresses, no wall-clock, no iteration over unordered
 * collections. Every outcome is a class simple name or a fixed literal.
 * Deliberately no lambdas inside the caller-sensitive helpers: the CALLER CLASS
 * is the thing under test, and a lambda body is hosted in a synthetic method
 * whose frame an implementation may resolve differently. The receiver-matrix
 * helpers are plain static methods with explicit try/catch for that reason.
 */
public class RJdkFieldModule {

    static int checks;

    interface Act {
        void run() throws Throwable;
    }

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    static String outcome(Act a) {
        try {
            a.run();
            return "OK";
        } catch (Throwable t) {
            return t.getClass().getSimpleName();
        }
    }

    static void expect(String label, String want, Act a) {
        String got = outcome(a);
        check(want.equals(got), label + ": expected " + want + " but got " + got);
    }

    // ------------------------------------------------------------------
    // 0. The module edges every row below depends on. If these move, the
    //    expectations are not wrong, they are meaningless -- so assert them.
    // ------------------------------------------------------------------
    static void moduleEdges() {
        Module base = Object.class.getModule();
        Module self = RJdkFieldModule.class.getModule();
        check(!self.isNamed(), "this vector must run from the class path (unnamed module)");
        check(base.isNamed() && "java.base".equals(base.getName()), "java.base must be named");

        check(base.isExported("java.lang", self), "java.base must export java.lang");
        check(!base.isOpen("java.lang", self), "java.base must NOT open java.lang");
        check(base.isExported("java.io", self), "java.base must export java.io");
        check(!base.isOpen("java.io", self), "java.base must NOT open java.io");
        check(base.isExported("java.util", self), "java.base must export java.util");
        check(!base.isOpen("java.util", self), "java.base must NOT open java.util");
        check(base.isExported("java.text", self), "java.base must export java.text");
        check(!base.isOpen("java.text", self), "java.base must NOT open java.text");
        check(!base.isExported("jdk.internal.misc", self),
                "java.base must NOT export jdk.internal.misc");
        check(!base.isOpen("jdk.internal.misc", self),
                "java.base must NOT open jdk.internal.misc");
        System.out.println("CK RJdkFieldModule edges lang=E io=E util=E text=E internal=none");
    }

    // ------------------------------------------------------------------
    // 1. ALLOW: public field, public class, EXPORTED-not-opened package.
    //    Static and instance, generic accessor and the whole typed family.
    // ------------------------------------------------------------------
    static void publicFieldExportedPackage() throws Exception {
        Field out = System.class.getField("out");
        check(Modifier.isPublic(out.getModifiers()), "System.out must be public");
        check(Modifier.isStatic(out.getModifiers()), "System.out must be static");
        // IDENTITY, not `!= null`. This VM's get-field-by-name path has a
        // recorded habit of answering a plausible default for a field it did
        // not resolve, and any non-null PrintStream satisfied the old check --
        // including one that is not the stream every other caller writes to.
        check(out.get(null) == System.out, "System.out must read through Field.get(null)");
        // A static field ignores the receiver rather than rejecting it, and
        // must answer the SAME object it answered for a null receiver.
        check(out.get(new Object()) == System.out,
                "a static Field.get must ignore its receiver");

        Field max = Integer.class.getField("MAX_VALUE");
        check(((Integer) max.get(null)) == Integer.MAX_VALUE, "Integer.MAX_VALUE via get");
        check(max.getInt(null) == Integer.MAX_VALUE, "Integer.MAX_VALUE via getInt");
        check(max.getLong(null) == Integer.MAX_VALUE, "Integer.MAX_VALUE via getLong");

        // Public INSTANCE field, non-final, in exported-not-opened java.io.
        InterruptedIOException e = new InterruptedIOException("x");
        e.bytesTransferred = 11;
        Field bt = InterruptedIOException.class.getField("bytesTransferred");
        check(!Modifier.isStatic(bt.getModifiers()), "bytesTransferred must be an instance field");
        check(((Integer) bt.get(e)) == 11, "instance public field via get");
        check(bt.getInt(e) == 11, "instance public field via getInt");
        check(bt.getLong(e) == 11L, "instance public field via getLong");
        check(bt.getFloat(e) == 11.0f, "instance public field via getFloat");
        check(bt.getDouble(e) == 11.0d, "instance public field via getDouble");
        bt.set(e, 12);
        check(e.bytesTransferred == 12, "instance public field via set");
        bt.setInt(e, 13);
        check(e.bytesTransferred == 13, "instance public field via setInt");
        bt.setByte(e, (byte) 14);
        check(e.bytesTransferred == 14, "instance public field via setByte (widening)");
        bt.setShort(e, (short) 15);
        check(e.bytesTransferred == 15, "instance public field via setShort (widening)");
        bt.setChar(e, (char) 16);
        check(e.bytesTransferred == 16, "instance public field via setChar (widening)");

        // The typed family still applies its own JLS widening matrix; a type
        // rejection here must be IllegalArgumentException, never an access
        // failure wearing the wrong name.
        expect("bytesTransferred.getBoolean", "IllegalArgumentException", () -> bt.getBoolean(e));
        expect("bytesTransferred.getByte", "IllegalArgumentException", () -> bt.getByte(e));
        expect("bytesTransferred.setLong", "IllegalArgumentException", () -> bt.setLong(e, 1L));

        // Reference and double public instance fields, same package.
        StreamTokenizer st = new StreamTokenizer(new StringReader("hi"));
        st.nextToken();
        Field sval = StreamTokenizer.class.getField("sval");
        Field nval = StreamTokenizer.class.getField("nval");
        Field ttype = StreamTokenizer.class.getField("ttype");
        check("hi".equals(sval.get(st)), "public reference field via get: " + sval.get(st));
        check(nval.getDouble(st) == 0.0d, "public double field via getDouble");
        check(ttype.getInt(st) == StreamTokenizer.TT_WORD, "public int field via getInt");

        // An instance field read with a null receiver is an NPE, not a denial.
        expect("bytesTransferred.get(null)", "NullPointerException", () -> bt.get(null));
        System.out.println("CK RJdkFieldModule allow public=exported static+instance+typed=ok");
    }

    // ------------------------------------------------------------------
    // 2. ALLOW: setAccessible on a public field of a public class in a
    //    merely EXPORTED package. The pair for row 4's InaccessibleObject.
    // ------------------------------------------------------------------
    static void setAccessibleOnPublicField() throws Exception {
        expect("System.out.setAccessible(true)", "OK",
                () -> System.class.getField("out").setAccessible(true));
        expect("bytesTransferred.setAccessible(true)", "OK",
                () -> InterruptedIOException.class.getField("bytesTransferred")
                        .setAccessible(true));
        System.out.println("CK RJdkFieldModule allow setAccessible-public-exported=ok");
    }

    // ------------------------------------------------------------------
    // 3. DENY: final fields are readable but not writable without an
    //    override, and static final is not writable even with one.
    // ------------------------------------------------------------------
    static void finalFields() throws Exception {
        Field out = System.class.getField("out");
        check(Modifier.isFinal(out.getModifiers()), "System.out must be final");
        expect("System.out.set", "IllegalAccessException", () -> out.set(null, null));
        expect("Integer.MAX_VALUE.setInt", "IllegalAccessException",
                () -> Integer.class.getField("MAX_VALUE").setInt(null, 1));
        System.out.println("CK RJdkFieldModule deny final-write=IllegalAccessException");
    }

    // ------------------------------------------------------------------
    // 4. DENY: NON-public field in an exported-not-opened package. The read
    //    is refused (IllegalAccessException) and the override that would
    //    lift it is itself refused (InaccessibleObjectException) -- two
    //    different exceptions from two different gates, both required.
    // ------------------------------------------------------------------
    static void nonPublicFieldExportedPackage() throws Exception {
        Field hash = String.class.getDeclaredField("hash");
        check(Modifier.isPrivate(hash.getModifiers()), "String.hash must be private");
        expect("String.hash.get", "IllegalAccessException", () -> hash.get("q"));
        expect("String.hash.getInt", "IllegalAccessException", () -> hash.getInt("q"));
        expect("String.hash.setInt", "IllegalAccessException", () -> hash.setInt("q", 1));
        expect("String.hash.getBoolean (denied AND wrong type)", "IllegalAccessException",
                () -> hash.getBoolean("q"));
        expect("String.hash.setAccessible(true)", "InaccessibleObjectException",
                () -> hash.setAccessible(true));
        // A refused setAccessible must leave the override clear, so the read
        // still fails afterwards.
        check(!hash.canAccess("q"), "a refused setAccessible must not grant access");
        expect("String.hash.get after refused setAccessible", "IllegalAccessException",
                () -> hash.get("q"));

        Field elems = ArrayList.class.getDeclaredField("elementData");
        expect("ArrayList.elementData.get", "IllegalAccessException",
                () -> elems.get(new ArrayList<String>()));
        System.out.println("CK RJdkFieldModule deny nonpublic-exported=IllegalAccessException"
                + " override=InaccessibleObjectException");
    }

    // ------------------------------------------------------------------
    // 4a. PRECEDENCE: a null receiver on an INSTANCE field is a
    //     NullPointerException, and it outranks every access and module
    //     refusal. Field.checkAccess is called as
    //       checkAccess(caller, clazz,
    //                   Modifier.isStatic(modifiers) ? null : obj.getClass(),
    //                   modifiers)
    //     so obj.getClass() is dereferenced while the ARGUMENT LIST is being
    //     built, before verifyMemberAccess is entered at all.
    //
    //     Section 5 below reads unexported STATIC fields with a null receiver
    //     and expects IllegalAccessException; that pair only means something
    //     if the instance rows here answer differently, and they do. An
    //     implementation that runs its access check first passes section 5
    //     and fails every row here.
    // ------------------------------------------------------------------
    static void nullReceiverOutranksTheAccessRefusal() throws Exception {
        // Denied because the field is PRIVATE and in another module.
        Field hash = String.class.getDeclaredField("hash");
        expect("String.hash.get(null)", "NullPointerException", () -> hash.get(null));
        expect("String.hash.getInt(null)", "NullPointerException", () -> hash.getInt(null));
        expect("String.hash.setInt(null)", "NullPointerException", () -> hash.setInt(null, 1));
        // ...and with the wrong accessor type on top of that, so three
        // different exceptions are in play and the NPE still wins.
        expect("String.hash.getBoolean(null)", "NullPointerException",
                () -> hash.getBoolean(null));
        // The same field with a NON-null receiver is the access refusal, which
        // is what makes the four rows above a precedence result and not just a
        // statement that null receivers throw.
        expect("String.hash.get(receiver)", "IllegalAccessException", () -> hash.get("q"));

        // Denied because the field is PACKAGE-PRIVATE.
        Field elems = ArrayList.class.getDeclaredField("elementData");
        expect("ArrayList.elementData.get(null)", "NullPointerException", () -> elems.get(null));

        // Denied because the CLASS is not public -- a refusal raised at a
        // different gate again, and it loses to the NPE just the same.
        Class<?> cb = Class.forName("java.text.CalendarBuilder");
        Field cbInstance = null;
        for (Field f : cb.getDeclaredFields()) {
            if (!Modifier.isStatic(f.getModifiers())) {
                cbInstance = f;
                break;
            }
        }
        check(cbInstance != null, "java.text.CalendarBuilder must declare an instance field");
        final Field cbf = cbInstance;
        expect("CalendarBuilder instance field .get(null)", "NullPointerException",
                () -> cbf.get(null));
        expect("CalendarBuilder instance field .get(receiver)", "IllegalAccessException",
                () -> cbf.get(cb.getDeclaredConstructor().newInstance()));

        // And a field the caller IS entitled to read behaves identically, so
        // the rule does not depend on the access answer at all.
        expect("bytesTransferred.getInt(null)", "NullPointerException",
                () -> InterruptedIOException.class.getField("bytesTransferred").getInt(null));

        // The NPE also outranks the FINAL-write refusal, which is raised at a
        // third gate again. Own.pubFinal is public and final in the unnamed
        // module, so nothing about access or modules is in question: with a
        // receiver it is IllegalAccessException, without one it is an NPE.
        Field pubFinal = Own.class.getField("pubFinal");
        check(Modifier.isFinal(pubFinal.getModifiers()), "Own.pubFinal must be final");
        expect("Own.pubFinal.set(receiver)", "IllegalAccessException",
                () -> pubFinal.set(new Own(), 9));
        expect("Own.pubFinal.set(null)", "NullPointerException", () -> pubFinal.set(null, 9));
        expect("Own.pubFinal.setInt(null)", "NullPointerException",
                () -> pubFinal.setInt(null, 9));
        System.out.println("CK RJdkFieldModule precedence null-receiver=NPE"
                + " beats IllegalAccessException beats IllegalArgumentException");
    }

    // ------------------------------------------------------------------
    // 5. DENY: package NEITHER exported NOR opened, PUBLIC field, PUBLIC
    //    class. This is the row a "public fields skip the check" shortcut
    //    gets wrong, and nothing else in the table catches it.
    // ------------------------------------------------------------------
    static void publicFieldUnexportedPackage() throws Exception {
        Class<?> unsafe = Class.forName("jdk.internal.misc.Unsafe");
        check(Modifier.isPublic(unsafe.getModifiers()), "jdk.internal.misc.Unsafe is public");
        Field off = unsafe.getDeclaredField("INVALID_FIELD_OFFSET");
        check(Modifier.isPublic(off.getModifiers()) && Modifier.isStatic(off.getModifiers()),
                "INVALID_FIELD_OFFSET must be public static");
        expect("Unsafe.INVALID_FIELD_OFFSET.get", "IllegalAccessException", () -> off.get(null));
        expect("Unsafe.INVALID_FIELD_OFFSET.getInt", "IllegalAccessException",
                () -> off.getInt(null));
        expect("Unsafe.INVALID_FIELD_OFFSET.setAccessible(true)", "InaccessibleObjectException",
                () -> off.setAccessible(true));

        Field theUnsafe = unsafe.getDeclaredField("theUnsafe");
        expect("Unsafe.theUnsafe.get", "IllegalAccessException", () -> theUnsafe.get(null));
        expect("Unsafe.theUnsafe.setAccessible(true)", "InaccessibleObjectException",
                () -> theUnsafe.setAccessible(true));
        System.out.println("CK RJdkFieldModule deny public-field-unexported"
                + "=IllegalAccessException");
    }

    // ------------------------------------------------------------------
    // 6. DENY: PUBLIC field of a NON-public class in an EXPORTED package.
    //    The modifiers say public twice over; the class access flag is the
    //    only thing that refuses.
    // ------------------------------------------------------------------
    static void publicFieldNonPublicClass() throws Exception {
        Class<?> cb = Class.forName("java.text.CalendarBuilder");
        check(!Modifier.isPublic(cb.getModifiers()),
                "java.text.CalendarBuilder must be package-private");
        Field wy = cb.getDeclaredField("WEEK_YEAR");
        check(Modifier.isPublic(wy.getModifiers()), "WEEK_YEAR must be public");
        expect("CalendarBuilder.WEEK_YEAR.get", "IllegalAccessException", () -> wy.get(null));
        expect("CalendarBuilder.WEEK_YEAR.getInt", "IllegalAccessException",
                () -> wy.getInt(null));
        expect("CalendarBuilder.WEEK_YEAR.setAccessible(true)", "InaccessibleObjectException",
                () -> wy.setAccessible(true));
        System.out.println("CK RJdkFieldModule deny public-field-nonpublic-class"
                + "=IllegalAccessException");
    }

    // ------------------------------------------------------------------
    // 7. Cross-module protected INSTANCE field, and the JLS 6.6.2.1 RECEIVER
    //    refinement of it. java.io is exported and not opened, so a gate that
    //    asks `opens` refuses a read HotSpot makes -- and a gate that asks
    //    only "is the caller a subclass" allows three reads HotSpot refuses.
    //
    //    Every method below is a plain static method of Sink, so the caller
    //    class the JDK resolves is Sink itself. No lambdas here on purpose.
    // ------------------------------------------------------------------
    static class Sink extends ByteArrayOutputStream {
        /** A subclass of the CALLER. Permitted receiver. */
        static class Deeper extends Sink {}

        /** A subclass of the DECLARING class that is NOT under the caller. */
        static class Sibling extends ByteArrayOutputStream {}

        static Field buf() throws Exception {
            return ByteArrayOutputStream.class.getDeclaredField("buf");
        }

        static Field count() throws Exception {
            return ByteArrayOutputStream.class.getDeclaredField("count");
        }

        static String get(Field f, Object receiver) {
            try {
                f.get(receiver);
                return "OK";
            } catch (Throwable t) {
                return t.getClass().getSimpleName();
            }
        }

        static String getInt(Field f, Object receiver) {
            try {
                f.getInt(receiver);
                return "OK";
            } catch (Throwable t) {
                return t.getClass().getSimpleName();
            }
        }

        static String getLong(Field f, Object receiver) {
            try {
                f.getLong(receiver);
                return "OK";
            } catch (Throwable t) {
                return t.getClass().getSimpleName();
            }
        }

        static String getBoolean(Field f, Object receiver) {
            try {
                f.getBoolean(receiver);
                return "OK";
            } catch (Throwable t) {
                return t.getClass().getSimpleName();
            }
        }

        static String setInt(Field f, Object receiver) {
            try {
                f.setInt(receiver, 0);
                return "OK";
            } catch (Throwable t) {
                return t.getClass().getSimpleName();
            }
        }

        static String set(Field f, Object receiver, Object v) {
            try {
                f.set(receiver, v);
                return "OK";
            } catch (Throwable t) {
                return t.getClass().getSimpleName();
            }
        }

        static String canAccess(Field f, Object receiver) {
            try {
                return String.valueOf(f.canAccess(receiver));
            } catch (Throwable t) {
                return t.getClass().getSimpleName();
            }
        }

        static String openBuf() {
            try {
                buf().setAccessible(true);
                return "OK";
            } catch (Throwable t) {
                return t.getClass().getSimpleName();
            }
        }
    }

    /** A caller in the same package that is NOT a subclass of the target. */
    static class NotASink {
        static String get(Field f, Object receiver) {
            try {
                f.get(receiver);
                return "OK";
            } catch (Throwable t) {
                return t.getClass().getSimpleName();
            }
        }
    }

    static void eq(String want, String got, String label) {
        check(want.equals(got), label + ": expected " + want + " but got " + got);
    }

    static void protectedFieldFromSubclass() throws Exception {
        Field buf = Sink.buf();
        Field count = Sink.count();
        check(Modifier.isProtected(buf.getModifiers()),
                "ByteArrayOutputStream.buf must be protected");
        check(Modifier.isProtected(count.getModifiers()),
                "ByteArrayOutputStream.count must be protected");
        check(!Modifier.isStatic(buf.getModifiers()), "buf must be an instance field");

        Object own = new Sink();
        Object deeper = new Sink.Deeper();
        Object superclass = new ByteArrayOutputStream();
        Object sibling = new Sink.Sibling();

        // ALLOW: the receiver is the caller's own class, or below it.
        eq("OK", Sink.get(buf, own), "protected buf through the caller's own class");
        eq("OK", Sink.get(buf, deeper), "protected buf through a subclass of the caller");

        // DENY: the receiver is the declaring class itself...
        eq("IllegalAccessException", Sink.get(buf, superclass),
                "protected buf through a bare superclass receiver");
        // ...or a SIBLING subclass. It IS a subclass of the declaring class and
        // that is not the test; it is not under the CALLER.
        eq("IllegalAccessException", Sink.get(buf, sibling),
                "protected buf through a sibling subclass");
        // ...or an object of an entirely unrelated class. Access loses to
        // nothing here: the refusal is IllegalAccessException, not the
        // IllegalArgumentException the wrong receiver type would earn once
        // access had been granted.
        eq("IllegalAccessException", Sink.get(buf, "not a stream"),
                "protected buf through an unrelated receiver");

        // A null receiver on an instance field is an NPE, and it outranks the
        // refusal -- this row is denied on the receiver rule as well.
        eq("NullPointerException", Sink.get(buf, null), "protected buf through a null receiver");

        // The whole typed family funnels through the same rule, in both
        // directions, for both getters and setters.
        eq("OK", Sink.getInt(count, own), "count.getInt through the caller's own class");
        eq("OK", Sink.getLong(count, own), "count.getLong through the caller's own class");
        eq("OK", Sink.setInt(count, own), "count.setInt through the caller's own class");
        eq("OK", Sink.set(count, own, 0), "count.set through the caller's own class");
        eq("IllegalAccessException", Sink.getInt(count, superclass),
                "count.getInt through a bare superclass receiver");
        eq("IllegalAccessException", Sink.getLong(count, superclass),
                "count.getLong through a bare superclass receiver");
        eq("IllegalAccessException", Sink.setInt(count, superclass),
                "count.setInt through a bare superclass receiver");
        eq("IllegalAccessException", Sink.set(count, superclass, 0),
                "count.set through a bare superclass receiver");
        eq("IllegalAccessException", Sink.getInt(count, sibling),
                "count.getInt through a sibling subclass");
        eq("NullPointerException", Sink.getInt(count, null),
                "count.getInt through a null receiver");

        // Precedence: access beats type. `count` is an int, so getBoolean is
        // always the wrong accessor -- but only the ALLOWED receiver gets to
        // hear about it.
        eq("IllegalArgumentException", Sink.getBoolean(count, own),
                "count.getBoolean on an allowed receiver is a TYPE error");
        eq("IllegalAccessException", Sink.getBoolean(count, superclass),
                "count.getBoolean on a denied receiver is an ACCESS error");

        // canAccess answers the same question without performing the read.
        eq("true", Sink.canAccess(buf, own), "canAccess through the caller's own class");
        eq("true", Sink.canAccess(buf, deeper), "canAccess through a subclass of the caller");
        eq("false", Sink.canAccess(buf, superclass), "canAccess through a bare superclass");
        eq("false", Sink.canAccess(buf, sibling), "canAccess through a sibling subclass");

        // The allowance is NOT deep access: the override is still refused,
        // because java.io is exported and not opened.
        eq("InaccessibleObjectException", Sink.openBuf(),
                "setAccessible on a protected java.io INSTANCE field");

        // A caller that is NOT a subclass gets nothing, through ANY receiver --
        // including one that would have been legal for the subclass caller.
        eq("IllegalAccessException", NotASink.get(buf, own),
                "non-subclass caller, receiver that the subclass caller may use");
        eq("IllegalAccessException", NotASink.get(buf, superclass),
                "non-subclass caller, bare superclass receiver");
        Field in = FilterInputStream.class.getDeclaredField("in");
        expect("FilterInputStream.in.get from a non-subclass", "IllegalAccessException",
                () -> in.get(new java.io.BufferedInputStream(InputStream.nullInputStream())));
        System.out.println("CK RJdkFieldModule protected-instance receiver-matrix"
                + " own=ok sub=ok super=IAE sibling=IAE null=NPE nonsubclass=IAE");
    }

    // ------------------------------------------------------------------
    // 7b. The refinement does NOT apply to a protected STATIC field: with no
    //     receiver there is no receiver type to narrow, and HotSpot passes a
    //     null targetClass. PipedInputStream.PIPE_SIZE is protected static
    //     final int, and java.io is exported and not opened -- so the DENY
    //     half is supplied by the caller instead of the receiver.
    // ------------------------------------------------------------------
    static class Pipe extends java.io.PipedInputStream {
        static class PipeSibling extends java.io.PipedInputStream {}

        static Field pipeSize() throws Exception {
            return java.io.PipedInputStream.class.getDeclaredField("PIPE_SIZE");
        }

        static String get(Object receiver) {
            try {
                pipeSize().get(receiver);
                return "OK";
            } catch (Throwable t) {
                return t.getClass().getSimpleName();
            }
        }

        static String getInt(Object receiver) {
            try {
                pipeSize().getInt(receiver);
                return "OK";
            } catch (Throwable t) {
                return t.getClass().getSimpleName();
            }
        }

        static String setInt() {
            try {
                pipeSize().setInt(null, 1);
                return "OK";
            } catch (Throwable t) {
                return t.getClass().getSimpleName();
            }
        }

        static String open() {
            try {
                pipeSize().setAccessible(true);
                return "OK";
            } catch (Throwable t) {
                return t.getClass().getSimpleName();
            }
        }
    }

    /** A caller that is NOT a subclass of PipedInputStream. */
    static class NotAPipe {
        static String get() {
            try {
                java.io.PipedInputStream.class.getDeclaredField("PIPE_SIZE").get(null);
                return "OK";
            } catch (Throwable t) {
                return t.getClass().getSimpleName();
            }
        }

        static String getInt() {
            try {
                java.io.PipedInputStream.class.getDeclaredField("PIPE_SIZE").getInt(null);
                return "OK";
            } catch (Throwable t) {
                return t.getClass().getSimpleName();
            }
        }

        /**
         * {@code checkCanSetAccessible}'s exported-package carve-out for a
         * {@code protected static} member is guarded by
         * {@code isSubclassOf(caller, declaringClass)} -- so the same call
         * {@code Pipe.open()} makes is refused from here.
         */
        static String open() {
            try {
                java.io.PipedInputStream.class.getDeclaredField("PIPE_SIZE").setAccessible(true);
                return "OK";
            } catch (Throwable t) {
                return t.getClass().getSimpleName();
            }
        }
    }

    static void protectedStaticFieldIgnoresTheReceiver() throws Exception {
        Field ps = Pipe.pipeSize();
        int mods = ps.getModifiers();
        check(Modifier.isProtected(mods), "PIPE_SIZE must be protected");
        check(Modifier.isStatic(mods), "PIPE_SIZE must be static");
        check(Modifier.isFinal(mods), "PIPE_SIZE must be final");

        // ALLOW through every receiver, including the ones the INSTANCE matrix
        // above refuses. If the refinement leaked onto statics, the sibling and
        // the bare Object rows would flip and nothing else would notice.
        eq("OK", Pipe.get(null), "protected static through a null receiver");
        eq("OK", Pipe.get(new Pipe()), "protected static through the caller's own class");
        eq("OK", Pipe.get(new java.io.PipedInputStream()),
                "protected static through a bare superclass receiver");
        eq("OK", Pipe.get(new Pipe.PipeSibling()),
                "protected static through a sibling subclass");
        eq("OK", Pipe.get(new Object()), "protected static through an unrelated receiver");
        eq("OK", Pipe.getInt(null), "protected static via getInt");

        // Still final, and still not writable without an override.
        eq("IllegalAccessException", Pipe.setInt(), "protected static final write");

        // setAccessible on a protected STATIC member IS granted to a subclass
        // caller by checkCanSetAccessible's exported-package carve-out -- the
        // pair for the InaccessibleObjectException the protected INSTANCE
        // field earns from the same caller in section 7.
        eq("OK", Pipe.open(), "setAccessible on a protected static exported field");
        // ...and the carve-out's conjunct the caller half is there to prove.
        // AccessibleObject.checkCanSetAccessible reads
        //   isClassPublic && isProtected && isStatic
        //                 && isSubclassOf(caller, declaringClass)
        // so a caller in a module the package is merely EXPORTED to, but which
        // is not a subclass, is refused the very same call. Without this row a
        // carve-out that drops the subclass conjunct passes the whole table.
        eq("InaccessibleObjectException", NotAPipe.open(),
                "setAccessible on a protected static field by a non-subclass caller");
        // The PUBLIC arm of the same carve-out has no subclass requirement --
        // section 2 opens System.out from RJdkFieldModule, which is a subclass
        // of nothing -- so the refusal above is about `protected static`
        // specifically and not about the caller being foreign.

        // DENY: the same field from a caller that is not a subclass. This is
        // what keeps the six ALLOW rows above from passing for the trivial
        // reason that nothing checks statics at all.
        eq("IllegalAccessException", NotAPipe.get(),
                "protected static read by a non-subclass caller");
        eq("IllegalAccessException", NotAPipe.getInt(),
                "protected static getInt by a non-subclass caller");
        System.out.println("CK RJdkFieldModule protected-static receiver-ignored"
                + " every-receiver=ok nonsubclass=IllegalAccessException");
    }

    // ------------------------------------------------------------------
    // 8. Control: the class path is the unnamed module, which exports and
    //    opens everything. A gate that denied here would break every
    //    framework, so the whole table needs this row to stay green.
    // ------------------------------------------------------------------
    public static class Own {
        public int pub = 1;
        public final int pubFinal = 2;
        public static int pubStatic = 3;
        private int priv = 4;
        /** static final: not writable through Field.set even WITH the override. */
        public static final int pubStaticFinal = 5;
        /** A reference field and its final twin: the rank-5 / rank-6 pair. */
        public String str = "a";
        public final String strFinal = "b";
        /**
         * Separate final fields for the override rows of section 9f. A Field
         * object handed out twice would carry the override from one row into
         * another; distinct fields make the rows independent of that.
         */
        public final int pubFinalOv = 6;
        public static final int pubStaticFinalOv = 7;

        static String readOwnPrivate() {
            return outcome(() -> {
                Own o = new Own();
                if (Own.class.getDeclaredField("priv").getInt(o) != 4) {
                    throw new IllegalStateException("wrong value");
                }
            });
        }
    }

    static void unnamedModule() throws Exception {
        Own o = new Own();
        Field pub = Own.class.getField("pub");
        check(((Integer) pub.get(o)) == 1, "unnamed-module public field read");
        pub.set(o, 7);
        check(o.pub == 7, "unnamed-module public field write");
        check(Own.class.getField("pubStatic").getInt(null) == 3,
                "unnamed-module public static field read");
        // Public but final: readable, not writable without an override.
        expect("Own.pubFinal.set", "IllegalAccessException",
                () -> Own.class.getField("pubFinal").set(o, 9));
        // The declaring class reaches its own private field with no override.
        check("OK".equals(Own.readOwnPrivate()),
                "a class must read its own private field reflectively, got "
                        + Own.readOwnPrivate());
        // Deep reflection into the unnamed module is granted.
        Field priv = Own.class.getDeclaredField("priv");
        expect("Own.priv.setAccessible(true)", "OK", () -> priv.setAccessible(true));
        priv.setAccessible(true);
        check(priv.getInt(o) == 4, "unnamed-module private field after setAccessible");
        // A package-private class in the same runtime package is reachable.
        check(SamePackage.readVisible() == 5, "same-package public field of a non-public class");
        System.out.println("CK RJdkFieldModule control unnamed=open own-private=ok");
    }

    // ------------------------------------------------------------------
    // 9. THE PRECEDENCE LATTICE, in full. Every row below has TWO things
    //    wrong at once -- that is the only way precedence is observable, and
    //    a row with one thing wrong asserts nothing about ordering.
    //
    //    Measured on Temurin 25.0.3. It is NOT a three-level ladder:
    //    IllegalAccessException occurs at TWO different ranks, and an
    //    implementation that collapses them fixes one pair while breaking
    //    another.
    //
    //      rank 1  NullPointerException      null receiver, INSTANCE field
    //      rank 2  IllegalAccessException    access denied (JLS + JPMS)
    //      rank 3  IllegalArgumentException  typed accessor vs descriptor
    //      rank 4  IllegalArgumentException  receiver is not an instance of
    //                                        the declaring class
    //      rank 5  IllegalAccessException    final write
    //      rank 6  IllegalArgumentException  value type, on generic set()
    //
    //    Ranks 1 and 2 are Field.checkAccess, and the `setAccessible` override
    //    SKIPS THEM BOTH -- which is why 9f exists and why rank 1 is not a
    //    rule anyone wrote: it falls out of dereferencing obj.getClass() while
    //    checkAccess's argument list is built.
    // ------------------------------------------------------------------

    /** 9a. Rank 1 over rank 3: a null receiver beats the wrong accessor. */
    static void rank1BeatsRank3() throws Exception {
        Field bt = InterruptedIOException.class.getField("bytesTransferred");
        InterruptedIOException e = new InterruptedIOException("x");
        // bytesTransferred is a public int the caller may read: nothing about
        // access is in question, so the ONLY two candidates are the null
        // receiver and the wrong accessor, and the null receiver wins.
        expect("bytesTransferred.getBoolean(e) [rank 3 alone]", "IllegalArgumentException",
                () -> bt.getBoolean(e));
        expect("bytesTransferred.getBoolean(null) [rank 1 beats rank 3]", "NullPointerException",
                () -> bt.getBoolean(null));
        expect("bytesTransferred.setLong(e, 1L) [rank 3 alone]", "IllegalArgumentException",
                () -> bt.setLong(e, 1L));
        expect("bytesTransferred.setLong(null, 1L) [rank 1 beats rank 3]", "NullPointerException",
                () -> bt.setLong(null, 1L));
        expect("bytesTransferred.setBoolean(null, true) [rank 1 beats rank 3]",
                "NullPointerException", () -> bt.setBoolean(null, true));
        System.out.println("CK RJdkFieldModule rank1 null-receiver beats rank3 accessor-type");
    }

    /** 9b. Rank 2 over ranks 3, 4 and 6: the access refusal beats everything. */
    static void rank2BeatsTheRest() throws Exception {
        Field hash = String.class.getDeclaredField("hash");
        // The WHOLE typed family on a denied field. One accessor answering
        // IllegalArgumentException while its siblings answer
        // IllegalAccessException is exactly the drift this row set exists for:
        // the descriptor gate must sit below the access check for all of them,
        // not just the one that happened to have a vector row.
        expect("String.hash.getBoolean", "IllegalAccessException", () -> hash.getBoolean("q"));
        expect("String.hash.getByte", "IllegalAccessException", () -> hash.getByte("q"));
        expect("String.hash.getChar", "IllegalAccessException", () -> hash.getChar("q"));
        expect("String.hash.getShort", "IllegalAccessException", () -> hash.getShort("q"));
        expect("String.hash.getFloat", "IllegalAccessException", () -> hash.getFloat("q"));
        expect("String.hash.getDouble", "IllegalAccessException", () -> hash.getDouble("q"));
        expect("String.hash.setBoolean", "IllegalAccessException",
                () -> hash.setBoolean("q", true));
        expect("String.hash.setLong", "IllegalAccessException", () -> hash.setLong("q", 1L));
        expect("String.hash.setFloat", "IllegalAccessException", () -> hash.setFloat("q", 1f));
        expect("String.hash.setDouble", "IllegalAccessException", () -> hash.setDouble("q", 1d));
        expect("String.hash.setChar", "IllegalAccessException", () -> hash.setChar("q", 'a'));
        expect("String.hash.setShort", "IllegalAccessException",
                () -> hash.setShort("q", (short) 1));
        // Rank 2 over rank 4 (wrong receiver TYPE) and rank 6 (wrong value).
        expect("String.hash.get(unrelated receiver) [rank 2 beats rank 4]",
                "IllegalAccessException", () -> hash.get(new Object()));
        expect("String.hash.set(receiver, null) [rank 2 beats rank 6]",
                "IllegalAccessException", () -> hash.set("q", null));

        // The same ordering at the two OTHER gates that raise rank 2, so this
        // is a statement about the rank and not about one code path: a public
        // static field of an UNEXPORTED package...
        Class<?> unsafe = Class.forName("jdk.internal.misc.Unsafe");
        Field off = unsafe.getDeclaredField("INVALID_FIELD_OFFSET");
        expect("Unsafe.INVALID_FIELD_OFFSET.getBoolean", "IllegalAccessException",
                () -> off.getBoolean(null));
        expect("Unsafe.INVALID_FIELD_OFFSET.setLong", "IllegalAccessException",
                () -> off.setLong(null, 1L));
        // ...and a public static field of a NON-PUBLIC class.
        Class<?> cb = Class.forName("java.text.CalendarBuilder");
        Field wy = cb.getDeclaredField("WEEK_YEAR");
        expect("CalendarBuilder.WEEK_YEAR.getBoolean", "IllegalAccessException",
                () -> wy.getBoolean(null));
        expect("CalendarBuilder.WEEK_YEAR.setLong", "IllegalAccessException",
                () -> wy.setLong(null, 1L));
        System.out.println("CK RJdkFieldModule rank2 access-refusal beats rank3 rank4 rank6");
    }

    /** 9c. Rank 3 over rank 5: the typed accessor's gate beats the final check. */
    static void rank3BeatsRank5() throws Exception {
        Own o = new Own();
        Field psf = Own.class.getField("pubStaticFinal");
        // The SAME public static final int, through two accessors. This pair is
        // the whole reason rank 3 cannot live below rank 5.
        expect("Own.pubStaticFinal.setInt [rank 5]", "IllegalAccessException",
                () -> psf.setInt(null, 9));
        expect("Own.pubStaticFinal.setLong [rank 3 beats rank 5]", "IllegalArgumentException",
                () -> psf.setLong(null, 9L));
        expect("Own.pubStaticFinal.setBoolean [rank 3 beats rank 5]", "IllegalArgumentException",
                () -> psf.setBoolean(null, true));
        // The same pair on a java.base field, so it is not a property of the
        // unnamed module.
        Field maxv = Integer.class.getField("MAX_VALUE");
        expect("Integer.MAX_VALUE.setInt [rank 5]", "IllegalAccessException",
                () -> maxv.setInt(null, 1));
        expect("Integer.MAX_VALUE.setBoolean [rank 3 beats rank 5]", "IllegalArgumentException",
                () -> maxv.setBoolean(null, true));
        // ...and on a REFERENCE field, where every primitive setter is rank 3.
        Field out = System.class.getField("out");
        expect("System.out.set(null) [rank 5]", "IllegalAccessException", () -> out.set(null, null));
        expect("System.out.setInt [rank 3 beats rank 5]", "IllegalArgumentException",
                () -> out.setInt(null, 1));

        // Instance final, same two accessors.
        Field pubFinal = Own.class.getField("pubFinal");
        expect("Own.pubFinal.setByte [rank 5]", "IllegalAccessException",
                () -> pubFinal.setByte(o, (byte) 1));
        expect("Own.pubFinal.setLong [rank 3 beats rank 5]", "IllegalArgumentException",
                () -> pubFinal.setLong(o, 1L));
        Field strFinal = Own.class.getField("strFinal");
        expect("Own.strFinal.setInt [rank 3 beats rank 5]", "IllegalArgumentException",
                () -> strFinal.setInt(o, 1));
        System.out.println("CK RJdkFieldModule rank3 accessor-gate beats rank5 final-write");
    }

    /** 9d. Rank 4: the receiver TYPE, which beats the final check but not access. */
    static void rank4ReceiverType() throws Exception {
        Own o = new Own();
        Field bt = InterruptedIOException.class.getField("bytesTransferred");
        // The field is public and readable; only the receiver is wrong.
        expect("bytesTransferred.get(unrelated receiver)", "IllegalArgumentException",
                () -> bt.get(new Object()));
        expect("bytesTransferred.getInt(unrelated receiver)", "IllegalArgumentException",
                () -> bt.getInt(new Object()));
        expect("bytesTransferred.setInt(unrelated receiver)", "IllegalArgumentException",
                () -> bt.setInt(new Object(), 1));
        expect("bytesTransferred.get(array receiver)", "IllegalArgumentException",
                () -> bt.get(new int[1]));

        // Rank 4 over rank 5, on the same field, differing only in the receiver.
        Field pubFinal = Own.class.getField("pubFinal");
        expect("Own.pubFinal.set(own receiver) [rank 5]", "IllegalAccessException",
                () -> pubFinal.set(o, 9));
        expect("Own.pubFinal.set(unrelated receiver) [rank 4 beats rank 5]",
                "IllegalArgumentException", () -> pubFinal.set("not an Own", 9));
        expect("Own.pubFinal.setByte(own receiver) [rank 5]", "IllegalAccessException",
                () -> pubFinal.setByte(o, (byte) 1));
        expect("Own.pubFinal.setByte(unrelated receiver) [rank 4 beats rank 5]",
                "IllegalArgumentException", () -> pubFinal.setByte("not an Own", (byte) 1));

        // A STATIC field has neither rank 1 nor rank 4: the receiver is ignored
        // outright rather than validated. Without these the rank-4 rows above
        // could be satisfied by a check that fires on every non-null receiver.
        Field maxv = Integer.class.getField("MAX_VALUE");
        check(maxv.getInt(new Object()) == Integer.MAX_VALUE,
                "a static field must ignore an unrelated receiver, not reject it");
        check(maxv.getInt(new int[1]) == Integer.MAX_VALUE,
                "a static field must ignore an array receiver too");
        expect("Integer.MAX_VALUE.getBoolean(unrelated) [rank 3, receiver ignored]",
                "IllegalArgumentException", () -> maxv.getBoolean(new Object()));
        System.out.println("CK RJdkFieldModule rank4 receiver-type beats rank5"
                + " static-ignores-receiver");
    }

    /** 9e. Rank 5 over rank 6: the final check beats the VALUE type check. */
    static void rank5BeatsRank6() throws Exception {
        Own o = new Own();
        Field pub = Own.class.getField("pub");
        Field pubFinal = Own.class.getField("pubFinal");
        Field str = Own.class.getField("str");
        Field strFinal = Own.class.getField("strFinal");
        Field psf = Own.class.getField("pubStaticFinal");
        // Each pair differs ONLY in `final`. The non-final half is what makes
        // the final half a precedence result rather than a restatement of
        // section 3.
        expect("Own.pub.set(null into int) [rank 6]", "IllegalArgumentException",
                () -> pub.set(o, null));
        expect("Own.pubFinal.set(null into int) [rank 5 beats rank 6]", "IllegalAccessException",
                () -> pubFinal.set(o, null));
        expect("Own.str.set(wrong value class) [rank 6]", "IllegalArgumentException",
                () -> str.set(o, Integer.valueOf(1)));
        expect("Own.strFinal.set(wrong value class) [rank 5 beats rank 6]",
                "IllegalAccessException", () -> strFinal.set(o, Integer.valueOf(1)));
        expect("Own.pubStaticFinal.set(wrong value class) [rank 5 beats rank 6]",
                "IllegalAccessException", () -> psf.set(null, "not an int"));
        expect("Own.pubStaticFinal.set(null into int) [rank 5 beats rank 6]",
                "IllegalAccessException", () -> psf.set(null, null));
        System.out.println("CK RJdkFieldModule rank5 final-write beats rank6 value-type");
    }

    /**
     * 9f. The override DELETES ranks 1 and 2, so rank 3 goes first.
     *
     * <p>{@code Field.getBoolean} is
     * {@code if (!override) checkAccess(caller, obj); return
     * getFieldAccessor(obj).getBoolean(obj);} -- and rank 1 lives inside
     * {@code checkAccess}'s argument list, not before it. So the same
     * expression answers two different exceptions depending on nothing but the
     * override flag, and an implementation that raises the
     * {@code NullPointerException} unconditionally fails the first row here
     * while passing every row in 9a.
     */
    static void theOverrideDeletesRanks1And2() throws Exception {
        Own o = new Own();
        Field priv = Own.class.getDeclaredField("priv");
        priv.setAccessible(true);
        check(priv.getInt(o) == 4, "the override must actually be granted for 9f to mean anything");
        expect("priv.getBoolean(null) WITH override [rank 3 beats the NPE]",
                "IllegalArgumentException", () -> priv.getBoolean(null));
        expect("priv.get(null) WITH override [no rank 3 to win, so rank 4 NPE]",
                "NullPointerException", () -> priv.get(null));
        expect("priv.get(unrelated receiver) WITH override [rank 4]", "IllegalArgumentException",
                () -> priv.get(new Object()));
        expect("priv.getBoolean(unrelated receiver) WITH override", "IllegalArgumentException",
                () -> priv.getBoolean(new Object()));
        // The contrasting row, WITHOUT an override, is 9a's
        // bytesTransferred.getBoolean(null) -> NullPointerException.

        // A final INSTANCE field IS writable once the override is set...
        Field pfOv = Own.class.getField("pubFinalOv");
        pfOv.setAccessible(true);
        expect("Own.pubFinalOv.setInt WITH override", "OK", () -> pfOv.setInt(o, 9));
        check(pfOv.getInt(o) == 9, "an overridden final instance write must take effect");
        expect("Own.pubFinalOv.setInt(null) WITH override [rank 4 NPE]", "NullPointerException",
                () -> pfOv.setInt(null, 9));
        expect("Own.pubFinalOv.setInt(unrelated) WITH override [rank 4]",
                "IllegalArgumentException", () -> pfOv.setInt("not an Own", 9));
        // ...but a STATIC final field is not, and rank 3 still outranks that.
        Field psfOv = Own.class.getField("pubStaticFinalOv");
        psfOv.setAccessible(true);
        expect("Own.pubStaticFinalOv.setInt WITH override [rank 5 survives]",
                "IllegalAccessException", () -> psfOv.setInt(null, 9));
        expect("Own.pubStaticFinalOv.setLong WITH override [rank 3 beats rank 5]",
                "IllegalArgumentException", () -> psfOv.setLong(null, 9L));
        System.out.println("CK RJdkFieldModule override deletes rank1+rank2"
                + " rank3-then-goes-first");
    }

    // ------------------------------------------------------------------
    // 10. The SAME JLS 6.6.2.1 receiver refinement, on Method.invoke.
    //
    //     Not a field row, and deliberately here anyway: Field.checkAccess and
    //     Method.checkAccess compute the identical third argument to
    //     Reflection.verifyMemberAccess --
    //       Modifier.isStatic(modifiers) ? null : obj.getClass()
    //     -- so the two paths share a rule, and a rule that is asserted on one
    //     path only is a rule that drifts.
    //
    //     Object.finalize() is the witness because it needs no fixtures at all:
    //     protected, in java.lang (a different MODULE), inherited by every
    //     class, and its body does nothing, so the ALLOW rows are a plain "OK"
    //     with no exception plumbing in the way.
    //
    //     Plain static methods, no lambdas: the CALLER CLASS is the thing under
    //     test, and a lambda body is hosted in a synthetic method whose frame an
    //     implementation may resolve differently.
    // ------------------------------------------------------------------
    static class Finalizes {
        /** A subclass of the CALLER. Permitted receiver. */
        static class Deeper extends Finalizes {}

        static java.lang.reflect.Method fin() throws Exception {
            return Object.class.getDeclaredMethod("finalize");
        }

        static String invoke(Object receiver) {
            try {
                fin().invoke(receiver);
                return "OK";
            } catch (java.lang.reflect.InvocationTargetException e) {
                return "ITE:" + e.getCause().getClass().getSimpleName();
            } catch (Throwable t) {
                return t.getClass().getSimpleName();
            }
        }

        static String canAccess(Object receiver) {
            try {
                return String.valueOf(fin().canAccess(receiver));
            } catch (Throwable t) {
                return t.getClass().getSimpleName();
            }
        }
    }

    /** A caller that is not a subclass of Finalizes (only of Object). */
    static class NotAFinalizes {
        static String invoke(Object receiver) {
            try {
                Object.class.getDeclaredMethod("finalize").invoke(receiver);
                return "OK";
            } catch (java.lang.reflect.InvocationTargetException e) {
                return "ITE:" + e.getCause().getClass().getSimpleName();
            } catch (Throwable t) {
                return t.getClass().getSimpleName();
            }
        }

        static String canAccess(Object receiver) {
            try {
                return String.valueOf(Object.class.getDeclaredMethod("finalize")
                        .canAccess(receiver));
            } catch (Throwable t) {
                return t.getClass().getSimpleName();
            }
        }
    }

    static void methodInvokeReceiverRefinement() throws Exception {
        java.lang.reflect.Method fin = Object.class.getDeclaredMethod("finalize");
        check(Modifier.isProtected(fin.getModifiers()), "Object.finalize must be protected");
        check(!Modifier.isStatic(fin.getModifiers()), "Object.finalize must be an instance method");

        // ALLOW: the receiver is the caller's own class, or below it.
        eq("OK", Finalizes.invoke(new Finalizes()),
                "Object.finalize through the caller's own class");
        eq("OK", Finalizes.invoke(new Finalizes.Deeper()),
                "Object.finalize through a subclass of the caller");
        // DENY: the declaring class itself, and an unrelated class.
        eq("IllegalAccessException", Finalizes.invoke(new Object()),
                "Object.finalize through a bare Object receiver");
        eq("IllegalAccessException", Finalizes.invoke("not a Finalizes"),
                "Object.finalize through an unrelated receiver");
        // The row that distinguishes "the caller is a subclass of the declaring
        // class" (true of EVERY class here, since the declaring class is Object)
        // from "the receiver is under the caller", which is the actual rule.
        eq("IllegalAccessException", NotAFinalizes.invoke(new Finalizes()),
                "a receiver that is legal for the OTHER caller only");
        eq("OK", NotAFinalizes.invoke(new NotAFinalizes()),
                "the second caller reaches its own instance");

        // canAccess answers the same question without performing the call.
        eq("true", Finalizes.canAccess(new Finalizes()), "canAccess through the caller's own class");
        eq("true", Finalizes.canAccess(new Finalizes.Deeper()),
                "canAccess through a subclass of the caller");
        eq("false", Finalizes.canAccess(new Object()), "canAccess through a bare Object");
        eq("false", Finalizes.canAccess("not a Finalizes"),
                "canAccess through an unrelated receiver");
        eq("false", NotAFinalizes.canAccess(new Finalizes()),
                "canAccess a receiver legal for the other caller only");
        System.out.println("CK RJdkFieldModule Method.invoke receiver-matrix"
                + " own=ok sub=ok Object=IAE unrelated=IAE cross-caller=IAE");
    }

    public static void main(String[] args) throws Exception {
        moduleEdges();
        publicFieldExportedPackage();
        setAccessibleOnPublicField();
        finalFields();
        nonPublicFieldExportedPackage();
        nullReceiverOutranksTheAccessRefusal();
        publicFieldUnexportedPackage();
        publicFieldNonPublicClass();
        protectedFieldFromSubclass();
        protectedStaticFieldIgnoresTheReceiver();
        unnamedModule();
        // Section 9 runs LAST: 9f sets the `setAccessible` override on three
        // Own fields, and an implementation that hands out a shared Field
        // object would otherwise leak that override into the rows above.
        rank1BeatsRank3();
        rank2BeatsTheRest();
        rank3BeatsRank5();
        rank4ReceiverType();
        rank5BeatsRank6();
        theOverrideDeletesRanks1And2();
        methodInvokeReceiverRefinement();
        System.out.println("CK RJdkFieldModule checks=" + checks);
        System.out.println("PASS RJdkFieldModule (" + checks + " checks)");
    }
}

/** A NON-public class in the vector's own (unnamed) runtime package. */
class SamePackage {
    public static int visible = 5;

    static int readVisible() throws Exception {
        return SamePackage.class.getField("visible").getInt(null);
    }
}
