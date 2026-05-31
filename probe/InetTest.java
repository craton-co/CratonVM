import java.net.*;
public class InetTest {
    static void t(String h){
        try { InetAddress a = InetAddress.getByName(h); System.out.println("getByName('"+h+"') = "+a+" (NO throw)"); }
        catch(UnknownHostException e){ System.out.println("getByName('"+h+"') threw UnknownHostException"); }
        catch(Throwable e){ System.out.println("getByName('"+h+"') threw "+e.getClass().getName()); }
    }
    public static void main(String[] a){
        t("260.1.1.1");      // invalid octet >255
        t("fffff::");         // invalid IPv6 group (5 hex digits)
        t("192.168.0.1");     // valid
        t("1.2.3.4.5");       // too many octets
    }
}
