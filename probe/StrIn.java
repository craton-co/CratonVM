import java.lang.reflect.*;
public class StrIn {
    public static void main(String[] a) throws Exception {
        String s = new String(new char[]{0xD801,0xDC01});
        Field vf = String.class.getDeclaredField("value"); vf.setAccessible(true);
        Field cf = String.class.getDeclaredField("coder"); cf.setAccessible(true);
        byte[] val = (byte[]) vf.get(s);
        byte coder = (byte) cf.get(s);
        System.out.print("coder="+coder+" value.len="+val.length+" bytes=");
        for(byte b: val) System.out.printf("%02x ", b & 0xff);
        System.out.println();
        System.out.println("length()="+s.length());
    }
}
