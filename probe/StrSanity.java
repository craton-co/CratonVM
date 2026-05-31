public class StrSanity {
    static int p=0,f=0;
    static void c(String n, boolean ok){ if(ok)p++; else {f++; System.out.println("FAIL "+n);} }
    public static void main(String[] a){
        c("ascii len", "hello".length()==5);
        c("ascii charAt", "hello".charAt(1)=='e');
        c("cjk len", "中文".length()==2);
        c("cjk charAt", "中文".charAt(0)==0x4E2D);
        c("emoji len", "😀".length()==2);  // U+1F600 surrogate pair
        c("emoji charAt0", "😀".charAt(0)==0xD83D);
        c("emoji charAt1", "😀".charAt(1)==0xDE00);
        c("empty", "".length()==0 && "".isEmpty());
        c("substring", "hello".substring(1,3).equals("el"));
        c("concat len", ("a"+"😀"+"b").length()==4);
        c("new String(cp)", new String(new char[]{'x','y'}).length()==2);
        System.out.println("PASS="+p+" FAIL="+f);
    }
}
