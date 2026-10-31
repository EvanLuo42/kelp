use kelp_cas::{Chunker, FastCdcChunker};
use kelp_testkit::pseudo_random;

#[test]
fn spans_cover_input() {
    let data = pseudo_random(3 << 20, 1);
    let spans = FastCdcChunker::default().chunk(&data);
    let mut next = 0;
    for s in &spans {
        assert_eq!(s.offset, next);
        next += s.len as u64;
    }
    assert_eq!(next, data.len() as u64);
}

#[test]
fn local_edit_keeps_most_chunks() {
    let chunker = FastCdcChunker::default();
    let a = pseudo_random(4 << 20, 7);
    let mut b = a.clone();
    b.splice(2_000_000..2_000_000, *b"inserted bytes");

    let ids = |d: &[u8]| {
        chunker
            .chunk(d)
            .iter()
            .map(|s| kelp_core::ChunkId::of(&d[s.offset as usize..][..s.len as usize]))
            .collect::<std::collections::HashSet<_>>()
    };
    let (ia, ib) = (ids(&a), ids(&b));
    let shared = ia.intersection(&ib).count();
    assert!(
        shared + 3 >= ia.len(),
        "only {shared}/{} chunks shared",
        ia.len()
    );
}
