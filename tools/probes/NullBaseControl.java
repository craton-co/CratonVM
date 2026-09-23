import java.lang.reflect.Field;

/** POSITIVE CONTROL for the R5 instrument.
 *
 *  After the latch repair, `UNCLASSIFIED-NULL-BASE` fell to 0 on the unmap
 *  repro and on both H2 vectors, which previously produced 3, 11 and 6 warns
 *  (occurrence reaching 513). A fall to zero is only evidence if the
 *  instrument can still fire -- this lane has three times been caught reading
 *  a mute instrument as a clean result.
 *
 *  So: make the access the instrument exists to count. A null base with an
 *  offset that is not an arena handle, not a synthetic offset and not a
 *  registered static field. It MUST warn. If it does not, every zero above is
 *  void and says nothing about the fix.
 *
 *  CratonVM only, deliberately: on HotSpot a null base is an absolute address
 *  and this SIGSEGVs. It is a control for the instrument, not a differential.
 */
public class NullBaseControl {
    public static void main(String[] args) throws Exception {
        Field f = Class.forName("sun.misc.Unsafe").getDeclaredField("theInternalUnsafe");
        f.setAccessible(true);
        jdk.internal.misc.Unsafe u = (jdk.internal.misc.Unsafe) f.get(null);

        // Offsets chosen to be nothing the classifier recognises.
        int a = u.getIntVolatile(null, 0x7654321L);
        boolean b = u.compareAndSetInt(null, 0x7654329L, 0, 1);
        System.out.println("control read |" + a + "| cas |" + b + "|");
        System.out.println("DONE NullBaseControl");
    }
}
