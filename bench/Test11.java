public class Test11 {
    static abstract class Shape {
        abstract double area();
    }
    static class Circle extends Shape {
        double radius;
        Circle(double r) { this.radius = r; }
        double area() { return Math.PI * radius * radius; }
    }

    public static void main(String[] args) {
        System.out.println("start");
        Shape circle = new Circle(5);
        System.out.println("circle created");
        double area = circle.area();
        System.out.println("area=" + area);
        System.out.println("DONE");
    }
}
