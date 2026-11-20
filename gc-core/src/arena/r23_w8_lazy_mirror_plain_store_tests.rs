use super::*;

pub fn run(edits: &[(&str, Option<&str>)]) -> (bool, Vec<u64>, Vec<(usize, usize)>) {
    let mut a = Arena::new(256 * 1024);
    cratonvm_types::flags::with_thread_overrides(edits, || {
        a.note_lazy_dirty(&[(4096, 8192), (20_000 & !7, 3000 & !7), (64 * 1024, 64 * 1024)]);
    });
    let plain = a.zero_sets.lazy_dirty.plain_stores;
    let mut hits = Vec::new();
    for &(o, l) in &[(4096 + 512, 1024), (19_000 & !7, 2048), (64 * 1024 + 8, 100 * 1024)] {
        if let Some(h) = a.take_lazy_dirty(o, l) {
            hits.push(h);
        }
    }
    a.restore_lazy_mirror_over(&[(0, 256 * 1024)]);
    let m = a.lazy_dirty_mirror().expect("a mirror");
    let words = (0..m.word_count()).map(|w| m.word_at(w)).collect();
    (plain, words, hits)
}

