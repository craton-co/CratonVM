import java.util.Date;
import java.io.*;
import java.math.BigInteger;

public class MiscProbe {
    public static void main(String[] a) {
        // 1. java.util.Date(String) / Date.parse — used by Spring's ObjectToObjectConverter
        String rfc = "Thu, 21 Apr 2016 17:11:08 +0100";
        try { System.out.println("1 new Date(String) = " + new Date(rfc).getTime()); }
        catch (Throwable t) { System.out.println("1 new Date(String) threw " + t); }
        try { System.out.println("2 Date.parse       = " + Date.parse(rfc)); }
        catch (Throwable t) { System.out.println("2 Date.parse threw " + t); }
        // 3. ObjectInputStream on garbage that looks like a stream referencing an undefined class
        BigInteger FOO = new BigInteger(
            "-9702942423549012526722364838327831379660941553432801565505143675386108883970811292563757558516603356009681061" +
            "5697574744209306031461371833798723505120163874786203211176873686513374052845353833564048");
        try {
            new ObjectInputStream(new ByteArrayInputStream(FOO.toByteArray())).readObject();
            System.out.println("3 readObject: no throw");
        } catch (Throwable t) { System.out.println("3 readObject threw " + t.getClass().getName() + ": " + t.getMessage()); }
    }
}
