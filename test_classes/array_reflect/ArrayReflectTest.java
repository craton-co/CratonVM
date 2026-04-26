import java.lang.reflect.Array;
public class ArrayReflectTest {
    public static void main(String[] args) {
        int[] ia = (int[]) Array.newInstance(int.class, 3);
        Array.setInt(ia, 0, 10);
        Array.setInt(ia, 1, 20);
        Array.setInt(ia, 2, 30);
        System.out.println("ia=[" + Array.getInt(ia, 0) + "," + Array.getInt(ia, 1) + "," + Array.getInt(ia, 2) + "]");
        Array.set(ia, 1, Integer.valueOf(99));
        System.out.println("ia[1]=" + Array.getInt(ia, 1));
        Object[] oa = (Object[]) Array.newInstance(String.class, 2);
        Array.set(oa, 0, "hello");
        Array.set(oa, 1, "world");
        System.out.println("oa=[" + Array.get(oa, 0) + "," + Array.get(oa, 1) + "]");
        System.out.println("len=" + Array.getLength(oa));
        System.out.println("OK");
    }
}
