import jakarta.persistence.*;
public class HibernateSmoke {
    public static void main(String[] args) {
        EntityManagerFactory f = Persistence.createEntityManagerFactory("smoke");
        EntityManager em = f.createEntityManager();
        em.getTransaction().begin();
        Note n = new Note();
        n.text = "hello";
        em.persist(n);
        em.getTransaction().commit();
        Note found = em.find(Note.class, n.id);
        System.out.println("HIB_SMOKE_OK text=" + found.text);
        em.close();
        f.close();
    }
}
