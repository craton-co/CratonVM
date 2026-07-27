import java.io.File;
import java.net.*;
import java.nio.file.*;
public class HashProbe {
  public static void main(String[] a) throws Exception {
    Path dir = Files.createTempDirectory("hashprobe");
    Path f = dir.resolve("resource#test1.txt");
    Files.writeString(f, "x");
    File file = f.toFile();
    System.out.println("1 file.toURI()          = " + file.toURI());
    URL u = file.toURI().toURL();
    System.out.println("2 toURL()               = " + u);
    System.out.println("3 url.getPath()         = " + u.getPath());
    System.out.println("4 url.getFile()         = " + u.getFile());
    System.out.println("5 new File(u.toURI())   = " + new File(u.toURI()));
    System.out.println("6 URLDecoder(%23)       = " + URLDecoder.decode("resource%23test1.txt", "UTF-8"));
    System.out.println("7 dir listing           = " + java.util.Arrays.toString(dir.toFile().list()));
    System.out.println("8 exists via URI        = " + new File(u.toURI()).exists());
  }
}
