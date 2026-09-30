// Standalone: what does enumerating this process's threads actually cost on
// Windows? Build and run with no cargo integration:
//
//     rustc -O -o snaptime.exe snaptime.rs && ./snaptime.exe
//
// Written for
// docs/internal/performance/xt-root-scan-enumerated-the-whole-machine-20260910.md,
// which needed the number independently of the VM: `CreateToolhelp32Snapshot`
// walks every thread on the MACHINE, so the cost scales with the box's total
// thread count and not with the VM's. On the dev box that was 6,952 threads to
// find 44, at 83 ms per snapshot+walk -- and `xt_root_scan` was paying it once
// per barrier round.
#[repr(C)]
#[derive(Clone, Copy)]
struct ThreadEntry32 {
    dw_size: u32, cnt_usage: u32, th32_thread_id: u32,
    th32_owner_process_id: u32, tp_base_pri: i32, tp_delta_pri: i32, dw_flags: u32,
}
extern "system" {
    fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> isize;
    fn Thread32First(snap: isize, e: *mut ThreadEntry32) -> i32;
    fn Thread32Next(snap: isize, e: *mut ThreadEntry32) -> i32;
    fn CloseHandle(h: isize) -> i32;
    fn GetCurrentProcessId() -> u32;
}
const TH32CS_SNAPTHREAD: u32 = 0x4;
fn main() {
    let pid = unsafe { GetCurrentProcessId() };
    // a few spare threads so the walk has something to find
    for _ in 0..40 { std::thread::spawn(|| std::thread::sleep(std::time::Duration::from_secs(60))); }
    std::thread::sleep(std::time::Duration::from_millis(200));
    let mut total = std::time::Duration::ZERO;
    let mut snap_only = std::time::Duration::ZERO;
    let n = 50;
    let (mut sys, mut mine) = (0usize, 0usize);
    for _ in 0..n {
        let t0 = std::time::Instant::now();
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        let t1 = std::time::Instant::now();
        let (mut s, mut m) = (0usize, 0usize);
        let mut e: ThreadEntry32 = unsafe { core::mem::zeroed() };
        e.dw_size = core::mem::size_of::<ThreadEntry32>() as u32;
        let mut ok = unsafe { Thread32First(snap, &mut e) };
        while ok != 0 { s += 1; if e.th32_owner_process_id == pid { m += 1; }
            ok = unsafe { Thread32Next(snap, &mut e) }; }
        unsafe { CloseHandle(snap) };
        total += t0.elapsed(); snap_only += t1 - t0; sys = s; mine = m;
    }
    println!("threads on system={sys} in this process={mine}");
    println!("snapshot alone : {:?} per call", snap_only / n);
    println!("snapshot+walk  : {:?} per call", total / n);
    println!("=> 174 passes  : {:?}", (total / n) * 174);
}
