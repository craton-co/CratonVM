public class CPC {
    public static void main(String[] a){
        String s = new String(new char[]{0xD801,0xDC01});
        System.out.println("length="+s.length()+" codePointCount="+s.codePointCount(0,s.length()));
    }
}
