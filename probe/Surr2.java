import java.nio.charset.StandardCharsets;
public class Surr2 {
    public static void main(String[] a){
        char[] ch = new char[]{0xD801,0xDC01};
        String s = new String(ch);
        System.out.println("char[].length="+ch.length+" String.length()="+s.length()+" codePointCount="+s.codePointCount(0,s.length()));
        byte[] b = s.getBytes(StandardCharsets.UTF_8);
        System.out.println("getBytes(UTF8).length="+b.length+" (expect 4)");
        // String.format with %02x trailing NUL check
        String f = String.format("%%%02x", 0xf0);
        System.out.println("format result length="+f.length()+" = '"+f+"' (expect 3 '%f0')");
    }
}
