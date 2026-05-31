public class StrS {
    public static void main(String[] a){
        String s = new String(new char[]{0xD801,0xDC01});
        System.out.println("RESULT length="+s.length());
    }
}
