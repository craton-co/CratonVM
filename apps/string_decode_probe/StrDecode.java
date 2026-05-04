public class StrDecode {
    public static void main(String[] a) {
        String latin = "abcde";
        String mixed = "abc" + (char) 0xE9;
        StringBuilder sb = new StringBuilder("xs=[");
        sb.append("a, b, c");
        sb.append("]");
        System.out.println("latin=" + latin);
        System.out.println("mixed=" + mixed);
        System.out.println("sb=" + sb);
        StringBuilder sbu = new StringBuilder("U=[");
        sbu.append("a, b, c");
        sbu.append("中");
        sbu.append("]");
        System.out.println("sbu=" + sbu);
        String concat = "xs=" + "[a, b, c]";
        System.out.println("concat=" + concat);
        StringBuilder sb2 = new StringBuilder();
        sb2.append("[a, b, c]");
        String fromSb = sb2.toString();
        System.out.println("xs=" + fromSb);
        System.out.println("OK");
    }
}
