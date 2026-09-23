import java.lang.reflect.Field;

/** Why does reflection on `jdk.internal.misc.Unsafe`'s array constants fail
 *  here but work on HotSpot?
 *
 *  `UnsafeConstAgree` read them as a sentinel on CratonVM. A sentinel is not a
 *  diagnosis: a MISSING field would mean this VM's `jdk.internal.misc.Unsafe`
 *  is not the real JDK class (a definition-of-done finding), while an access
 *  failure is a probe artefact. Those two must not be conflated, so this
 *  prints the throwable's CLASS rather than a value.
 *
 *  `post_clinit_fixup`'s `jdk/internal/misc/Unsafe` arm sets these same 18
 *  fields by name and logs how many it found ("populated (n/18)"), so the
 *  fields are expected to exist. If they do, the failure is access, and the
 *  agreement invariant needs a different route to read them.
 */
public class InternalFieldProbe {

    static void probe(String cls, String field) {
        String outcome;
        try {
            Class<?> c = Class.forName(cls);
            Field f = c.getDeclaredField(field);
            outcome = "declared";
            try {
                f.setAccessible(true);
                Object v = f.get(null);
                outcome = "readable(" + (v == null ? "null" : v.getClass().getSimpleName()) + ")";
            } catch (Throwable t) {
                outcome = "declared-but-" + t.getClass().getSimpleName();
            }
        } catch (Throwable t) {
            outcome = "lookup-" + t.getClass().getSimpleName();
        }
        System.out.println(cls + "#" + field + " |" + outcome + "|");
    }

    public static void main(String[] args) {
        for (String f : new String[] {
                "ARRAY_BYTE_BASE_OFFSET", "ARRAY_INT_BASE_OFFSET",
                "ARRAY_OBJECT_BASE_OFFSET", "ARRAY_BYTE_INDEX_SCALE",
                "ARRAY_OBJECT_INDEX_SCALE", "ADDRESS_SIZE" }) {
            probe("jdk.internal.misc.Unsafe", f);
        }
        // Control: the SAME field names on the legacy spelling, which
        // `UnsafeConstAgree` read successfully on this VM. If these are
        // readable and the internal ones are not, the difference is the class,
        // not the probe.
        for (String f : new String[] {
                "ARRAY_BYTE_BASE_OFFSET", "ARRAY_OBJECT_INDEX_SCALE", "ADDRESS_SIZE" }) {
            probe("sun.misc.Unsafe", f);
        }
        // Second control: a field on the internal class that is NOT part of
        // the constant family, to separate "this class is unreadable" from
        // "these fields are absent".
        probe("jdk.internal.misc.Unsafe", "theUnsafe");
        System.out.println("DONE InternalFieldProbe");
    }
}
