import java.lang.invoke.*; import java.nio.ByteOrder;
public class Vh1 { public static void main(String[] a) throws Throwable {
 VarHandle s = MethodHandles.byteArrayViewVarHandle(short[].class, ByteOrder.BIG_ENDIAN);
 VarHandle i = MethodHandles.byteArrayViewVarHandle(int[].class, ByteOrder.LITTLE_ENDIAN);
 byte[] b = new byte[16];
 try { s.set(b, 0, (short) 0x1234); System.out.println("short ok " + b[0] + "," + b[1]); } catch (Throwable t) { System.out.println("short " + t); }
 try { i.set(b, 4, 0x01020304); System.out.println("int ok " + b[4] + "," + b[7]); } catch (Throwable t) { System.out.println("int " + t); }
 try { System.out.println((short) s.get(b, 0)); System.out.println((int) i.get(b, 4)); } catch (Throwable t) { System.out.println("get " + t); }
 }}
