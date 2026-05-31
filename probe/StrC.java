import java.lang.reflect.*;
public class StrC {
    public static void main(String[] a) throws Exception {
        String s = new String(new char[]{0xD801,0xDC01});
        System.out.println("length()="+s.length());
        Method cm = String.class.getDeclaredMethod("coder"); cm.setAccessible(true);
        System.out.println("coder()="+cm.invoke(s));
    }
}
