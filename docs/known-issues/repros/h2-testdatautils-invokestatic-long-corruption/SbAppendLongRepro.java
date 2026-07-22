public class SbAppendLongRepro {
    public static void main(String[] a) {
        long i = 1125899906842623L;
        long negI = -i;
        System.out.println("negI hex=" + Long.toHexString(negI));
        System.out.println("-i=" + negI);
        StringBuilder sb = new StringBuilder();
        sb.append(negI);
        System.out.println("sb=" + sb.toString());
        System.out.println("String.valueOf(negI)=" + String.valueOf(negI));
    }
}
