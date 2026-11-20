use super::*;

fn strings(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|s| s.to_string()).collect()
}

#[test]
fn i137_l1b_vm_flags_prints_the_set_flags_on_one_line() {
    let set = strings(&["-XX:InitialHeapSize=268435456", "-XX:+UseG1GC"]);
    assert_eq!(
        vm_flags_dcmd_text(false, &set, &[]),
        "-XX:InitialHeapSize=268435456 -XX:+UseG1GC \n"
    );
    assert_eq!(vm_flags_dcmd_text(false, &[], &[]), "\n");
}

#[test]
fn i137_l1b_vm_flags_all_prints_the_final_table() {
    let finals = strings(&["   size_t MaxHeapSize = 1", "     bool UseG1GC = true"]);
    assert_eq!(
        vm_flags_dcmd_text(true, &strings(&["-XX:+UseG1GC"]), &finals),
        "[Global flags]\n   size_t MaxHeapSize = 1\n     bool UseG1GC = true\n"
    );
    assert!(vm_flags_all_option(&strings(&["-all"])));
    assert!(vm_flags_all_option(&strings(&["-all=true"])));
    assert!(!vm_flags_all_option(&strings(&["-all", "-all=false"])));
    assert!(!vm_flags_all_option(&[]));
}
