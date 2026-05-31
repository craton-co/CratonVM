public class StrLen {
    public static void main(String[] a){
        char[] ch = new char[]{0xD801,0xDC01};
        String s = new String(ch);
        System.out.println("length="+s.length()+" (expect 2)");
        System.out.println("charAt(0)="+(int)s.charAt(0)+" charAt(1)="+(int)s.charAt(1));
        // simple BMP control
        String s2 = new String(new char[]{'A','B','C'});
        System.out.println("s2.length="+s2.length()+" (expect 3)");
        // high chars
        String s3 = new String(new char[]{0x4E2D,0x6587}); // CJK
        System.out.println("s3.length="+s3.length()+" (expect 2)");
    }
}
