import java.nio.*;
import java.lang.reflect.*;
public class UAddr {
  public static void main(String[] a) throws Exception {
    Class<?> uc = Class.forName("sun.misc.Unsafe");
    Field uf = uc.getDeclaredField("theUnsafe"); uf.setAccessible(true); Object u = uf.get(null);
    Method objFieldOffset = uc.getMethod("objectFieldOffset", Field.class);
    Method getLong = uc.getMethod("getLong", Object.class, long.class);
    ByteBuffer bb = ByteBuffer.allocateDirect(64);
    Field addrF = Buffer.class.getDeclaredField("address");
    // reflective read (known good):
    addrF.setAccessible(true);
    System.out.println("reflective Field.getLong(address) = 0x"+Long.toHexString(addrF.getLong(bb)));
    // Unsafe path (what Netty uses):
    long off = (Long) objFieldOffset.invoke(u, addrF);
    System.out.println("Unsafe.objectFieldOffset(address) = "+off+" (0x"+Long.toHexString(off)+")");
    long viaUnsafe = (Long) getLong.invoke(u, bb, off);
    System.out.println("Unsafe.getLong(bb, off)           = 0x"+Long.toHexString(viaUnsafe));
    // also: capacity field offset for sanity (another Buffer field)
    Field capF = Buffer.class.getDeclaredField("capacity");
    long capOff = (Long) objFieldOffset.invoke(u, capF);
    System.out.println("Unsafe.objectFieldOffset(capacity)= "+capOff);
    Method getInt = uc.getMethod("getInt", Object.class, long.class);
    System.out.println("Unsafe.getInt(bb, capOff)         = "+getInt.invoke(u, bb, capOff)+" (expect 64)");
    System.out.println("END");
  }
}
