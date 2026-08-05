import java.nio.ByteBuffer;
import java.util.Objects;

/** The NON-String callers of Preconditions: they must get IndexOutOfBoundsException,
 *  never the ArrayIndexOutOfBoundsException SUBCLASS. The matrix cannot see this -
 *  every one of its OOB rows is String-domain. */
public class NioOutOfBoundsClassProbe {
    static void t(String label, Runnable r) {
        try {
            r.run();
            System.out.println(label + " => NO-THROW");
        } catch (Throwable e) {
            System.out.println(label + " => " + e.getClass().getName() + " msg=" + e.getMessage());
        }
    }
    public static void main(String[] a) {
        byte[] arr = new byte[8];
        t("Objects.checkIndex(9,8)      ", () -> Objects.checkIndex(9, 8));
        t("Objects.checkFromToIndex(3,2)", () -> Objects.checkFromToIndex(3, 2, 8));
        t("Objects.checkFromIndexSize   ", () -> Objects.checkFromIndexSize(6, 5, 8));
        ByteBuffer bb = ByteBuffer.wrap(arr);
        t("ByteBuffer.get(99)           ", () -> bb.get(99));
        t("List.of().get(3)             ", () -> java.util.List.of("a").get(3));
        System.out.println("NIO-OOB-PROBE-DONE");
    }
}
