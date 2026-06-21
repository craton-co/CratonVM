public class RejectRefArray {
    public static int sum(Integer[] a) {
        int s = 0;
        for (Integer v : a) s += v.intValue();
        return s;
    }
}
