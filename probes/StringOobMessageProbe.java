/**
 * Out-of-bounds exception CLASS and MESSAGE for the String domain, against a
 * HotSpot control.
 *
 * Written because StringPolicyMatrixProbe showed only a null message for
 * charAt and that hid a worse defect: before 2026-08-05 the exception CLASS
 * depended on the SIGN of the index. charAt(-1) reached
 * Preconditions.checkIndex and came back ArrayIndexOutOfBoundsException, while
 * charAt(12) came back StringIndexOutOfBoundsException. A catch of
 * StringIndexOutOfBoundsException therefore worked in one direction only.
 *
 * Print the class and the message, never just "threw": the class is the half
 * that changes control flow, and a matrix row that already differs on message
 * text cannot show the class changing underneath it.
 */
public class StringOobMessageProbe {
    static void t(String label, Runnable r) {
        try {
            r.run();
            System.out.println(label + " => NO-THROW");
        } catch (Throwable e) {
            System.out.println(label + " => " + e.getClass().getName() + " msg=" + e.getMessage());
        }
    }
    public static void main(String[] a) {
        String s = "hello world!";           // length 12
        t("charAt(-1)      ", () -> s.charAt(-1));
        t("charAt(12)      ", () -> s.charAt(12));
        t("substring(-1)   ", () -> s.substring(-1));
        t("substring(3,2)  ", () -> s.substring(3, 2));
        StringBuilder sb = new StringBuilder("abc");
        t("sb.charAt(9)    ", () -> sb.charAt(9));
        System.out.println("OOB-PROBE-DONE");
    }
}
