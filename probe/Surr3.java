import org.apache.tomcat.util.buf.UEncoder;
import org.apache.tomcat.util.buf.UEncoder.SafeCharsSet;
import java.io.IOException;
public class Surr3 {
    public static void main(String[] a) throws IOException {
        UEncoder enc = new UEncoder(SafeCharsSet.WITH_SLASH);
        String s = "a+b/c/d+e.class";
        String r1 = enc.encodeURL(s, 0, s.length()).toString();
        System.out.println("r1='"+r1+"' expect 'a%2bb/c/d%2be.class' match="+r1.equals("a%2bb/c/d%2be.class"));
        String r2 = enc.encodeURL(s, 2, s.length()-2).toString();
        System.out.println("r2='"+r2+"' expect 'b/c/d%2be.cla' match="+r2.equals("b/c/d%2be.cla"));
        String sp = new String(new char[]{0xD801,0xDC01});
        String r3 = enc.encodeURL(sp, 0, sp.length()).toString();
        System.out.println("r3='"+r3+"' expect '%f0%90%90%81' match="+r3.equals("%f0%90%90%81"));
    }
}
