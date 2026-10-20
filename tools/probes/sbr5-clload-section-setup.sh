cd /data/sbr5/clload; J=/data/jdkimages/jdk25-linux/jdk-25.0.4+7/bin; rm -rf sect; mkdir -p sect/s/com/se sect/o
printf "package com.se; public class P { }\n" > sect/s/com/se/P.java
printf "package com.se; public class Q { }\n" > sect/s/com/se/Q.java
$J/javac -d sect/o sect/s/com/se/*.java
printf "Manifest-Version: 1.0\nImplementation-Version: main1\nSpecification-Title: MainSpec\n\nName: com/se/\nImplementation-Version: sec2\nImplementation-Title: SecTitle\n" > sect/m.txt
$J/jar --create --file sect/sect.jar --manifest sect/m.txt -C sect/o .
cat > sect/Sbr5ClloadSection.java <<X
public class Sbr5ClloadSection { public static void main(String[] a) throws Exception {
  ClassLoader l = ClassLoader.getSystemClassLoader();
  Class<?> p = Class.forName("com.se.P", false, l); Package k = p.getPackage();
  System.out.println("impl=" + k.getImplementationVersion() + " title=" + k.getImplementationTitle() + " spec=" + k.getSpecificationTitle() + " sealed=" + k.isSealed());
  Package q = Class.forName("com.se.Q", false, l).getPackage(); System.out.println("same=" + (q == k)); } }
X
$J/javac -d sect sect/Sbr5ClloadSection.java
