import java.nio.*;
import java.lang.reflect.*;
public class DirectMem {
  static void t(String n, Runnable r){ try { r.run(); System.out.println(n+" OK"); } catch(Throwable e){ Throwable c=e.getCause()!=null?e.getCause():e; System.out.println(n+" THREW "+c.getClass().getName()+": "+c.getMessage()); } }
  public static void main(String[] a) throws Exception {
    t("ByteBuffer.allocateDirect write/read", () -> {
      ByteBuffer bb = ByteBuffer.allocateDirect(64);
      bb.putInt(0, 0x12345678);
      System.out.println("  get=0x"+Integer.toHexString(bb.getInt(0))+" isDirect="+bb.isDirect());
    });
    t("Buffer.address field", () -> {
      try { ByteBuffer bb = ByteBuffer.allocateDirect(64);
        Field f = Buffer.class.getDeclaredField("address"); f.setAccessible(true);
        System.out.println("  address=0x"+Long.toHexString(f.getLong(bb)));
      } catch(Exception e){ throw new RuntimeException(e); } });
    t("Unsafe allocateMemory/putByte", () -> {
      try { Class<?> uc = Class.forName("sun.misc.Unsafe");
        Field uf = uc.getDeclaredField("theUnsafe"); uf.setAccessible(true); Object u = uf.get(null);
        long addr = (Long) uc.getMethod("allocateMemory", long.class).invoke(u, 64L);
        System.out.println("  allocateMemory=0x"+Long.toHexString(addr));
        uc.getMethod("putByte", long.class, byte.class).invoke(u, addr, (byte)42);
        byte got = (Byte) uc.getMethod("getByte", long.class).invoke(u, addr);
        System.out.println("  put/get="+got);
        uc.getMethod("freeMemory", long.class).invoke(u, addr);
      } catch(Exception e){ throw new RuntimeException(e.getCause()!=null?e.getCause():e); } });
    System.out.println("DONE-DM");
  }
}
