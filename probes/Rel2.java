import java.nio.file.*;
public class Rel2 { public static void main(String[] a){ System.out.println(Paths.get("/A/b").relativize(Paths.get("/a/x"))); }}
