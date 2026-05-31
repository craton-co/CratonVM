import java.nio.*;
public class BufEx {
    public static void main(String[] a) {
        CharBuffer cb = CharBuffer.allocate(1);
        cb.put('X'); cb.flip(); cb.get();  // now position==limit==1
        System.out.println("pos="+cb.position()+" limit="+cb.limit());
        try { cb.get(); System.out.println("NO EXCEPTION (bug)"); }
        catch (BufferUnderflowException e) { System.out.println("BufferUnderflowException (correct)"); }
        catch (Throwable t) { System.out.println("WRONG TYPE: "+t.getClass().getName()+": "+t.getMessage()); }
        // also test empty decode result get
        CharBuffer empty = CharBuffer.allocate(0);
        try { empty.get(); System.out.println("empty: NO EXCEPTION (bug)"); }
        catch (BufferUnderflowException e) { System.out.println("empty: BufferUnderflowException (correct)"); }
        catch (Throwable t) { System.out.println("empty WRONG TYPE: "+t.getClass().getName()); }
    }
}
