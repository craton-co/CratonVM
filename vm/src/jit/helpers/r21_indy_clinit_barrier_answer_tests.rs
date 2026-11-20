use super::*;

/// The thread-id offset is `JvmThread::thread_id`'s plus one; class id 0
/// and an initialized class answer `0`; any other code is not ours.
#[test]
fn the_clinit_barrier_words_are_answered_only_for_their_codes() {
    use cratonvm_jit::ir_lower as jl;
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let vm: &SharedVm = &shared;
    assert_eq!(
        clinit_barrier_well_known_answer(vm, jl::WELL_KNOWN_JVM_THREAD_ID_OFFSET),
        Some(std::mem::offset_of!(JvmThread, thread_id) as i64 + 1)
    );
    assert_eq!(
        clinit_barrier_well_known_answer(
            vm,
            jl::vh_const_query(jl::WELL_KNOWN_CLINIT_OWNER_CELL_TAG, 0, 0)
        ),
        Some(0),
        "class id 0"
    );
    assert_eq!(
        clinit_barrier_well_known_answer(
            vm,
            jl::vh_const_query(jl::WELL_KNOWN_CLINIT_BASE_CELL_TAG, 0, 3)
        ),
        Some(0)
    );
    assert_eq!(
        clinit_barrier_well_known_answer(
            vm,
            jl::vh_const_query(jl::WELL_KNOWN_VH_CONST_PLAN_TAG, 7, 0)
        ),
        None,
        "a VarHandle query is not ours"
    );
    assert_eq!(clinit_barrier_well_known_answer(vm, 1), None);
}
