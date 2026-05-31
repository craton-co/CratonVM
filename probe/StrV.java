import java.lang.reflect.*;
public class StrV {
    public static void main(String[] a) throws Exception {
        String s = new String(new char[]{0xD801,0xDC01});
        Field vf = String.class.getDeclaredField("value"); vf.setAccessible(true);
        Field cf = String.class.getDeclaredField("coder"); cf.setAccessible(true);
        byte[] v = (byte[]) vf.get(s);
        System.out.println("value.length="+v.length+" coder="+cf.get(s)+" length()="+s.length());
        StringBuilder sb=new StringBuilder(); for(byte b:v) sb.append(String.format("%02x ",b&0xff));
        System.out.println("bytes="+sb);
    }
}
