use std::path::{Path, PathBuf};

use kelp_cas::{Chunker, FastCdcChunker};
use kelp_client::{ContentCache, Error, is_read_only};
use kelp_core::{ContentId, FileManifest};
use kelp_testkit::{Sandbox, pseudo_random};

/// Big enough to be written by several workers at once.
const LARGE: usize = 24 << 20;

struct Fixture {
    sandbox: Sandbox,
    cache: ContentCache,
}

impl Fixture {
    fn new() -> Self {
        let sandbox = Sandbox::new();
        let cache = ContentCache::new(sandbox.dir("cache"));
        Self { sandbox, cache }
    }

    async fn ingest(&self, data: &[u8]) -> FileManifest {
        // Small chunks, so that there are many of them.
        let chunker = FastCdcChunker::default();
        let manifest = self.cache.ingest_with(data, &chunker as &dyn Chunker).await;
        manifest.unwrap()
    }

    fn dest(&self, rel: &str) -> PathBuf {
        self.sandbox.root().join("ws").join(rel)
    }

    fn tmp(&self) -> PathBuf {
        self.sandbox.root().join("tmp")
    }

    /// Returns the files left in the temporary directory.
    fn leftovers(&self) -> Vec<PathBuf> {
        std::fs::read_dir(self.tmp())
            .map(|d| d.map(|e| e.unwrap().path()).collect())
            .unwrap_or_default()
    }

    /// Returns the cache file holding `chunk`.
    fn chunk_file(&self, manifest: &FileManifest, index: usize) -> PathBuf {
        let name = manifest.chunks[index].id.to_string();
        let root = self.sandbox.root().join("cache").join("chunks");
        find(&root, &name).expect("chunk is cached")
    }
}

fn find(dir: &Path, name: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()? {
        let path = entry.unwrap().path();
        if path.is_dir() {
            if let Some(found) = find(&path, name) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|n| n == name) {
            return Some(path);
        }
    }
    None
}

async fn materialize(
    fx: &Fixture,
    content: &ContentId,
    executable: bool,
    rel: &str,
) -> Result<(), Error> {
    fx.cache
        .materialize(content, executable, &fx.dest(rel), &fx.tmp())
        .await
}

#[tokio::test]
async fn large_multi_chunk_file_round_trips() {
    let fx = Fixture::new();
    let data = pseudo_random(LARGE, 7);
    let manifest = fx.ingest(&data).await;
    assert!(
        manifest.chunks.len() > 100,
        "{} chunks",
        manifest.chunks.len()
    );

    materialize(&fx, &manifest.content, false, "deep/dir/big.bin")
        .await
        .unwrap();

    let written = std::fs::read(fx.dest("deep/dir/big.bin")).unwrap();
    assert!(written == data, "content differs");
    assert!(fx.leftovers().is_empty(), "{:?}", fx.leftovers());
}

#[tokio::test]
async fn small_files_round_trip() {
    let fx = Fixture::new();
    let single = fx.ingest(b"hello").await;
    let empty = fx.ingest(b"").await;
    // Several chunks, but below the size that is worth parallel writers.
    let few = pseudo_random(2 << 20, 3);
    let few_manifest = fx.ingest(&few).await;
    assert!(few_manifest.chunks.len() > 1);

    materialize(&fx, &single.content, false, "single")
        .await
        .unwrap();
    materialize(&fx, &empty.content, false, "empty")
        .await
        .unwrap();
    materialize(&fx, &few_manifest.content, false, "few")
        .await
        .unwrap();

    assert_eq!(std::fs::read(fx.dest("single")).unwrap(), b"hello");
    assert_eq!(std::fs::read(fx.dest("empty")).unwrap(), b"");
    assert!(std::fs::read(fx.dest("few")).unwrap() == few);
    assert!(fx.leftovers().is_empty());
}

#[tokio::test]
async fn missing_chunk_fails_without_leaving_files() {
    for size in [LARGE, 2 << 20] {
        let fx = Fixture::new();
        let manifest = fx.ingest(&pseudo_random(size, 11)).await;
        let last = manifest.chunks.len() - 1;
        std::fs::remove_file(fx.chunk_file(&manifest, last)).unwrap();

        let err = materialize(&fx, &manifest.content, false, "big.bin")
            .await
            .unwrap_err();

        assert!(
            matches!(err, Error::NotCached(c) if c == manifest.content),
            "{err:?}"
        );
        assert!(!fx.dest("big.bin").exists());
        assert!(fx.leftovers().is_empty(), "{:?}", fx.leftovers());
    }
}

#[tokio::test]
async fn missing_manifest_is_not_cached() {
    let fx = Fixture::new();
    let other = fx.ingest(&pseudo_random(LARGE, 1)).await;
    let fresh = Fixture::new();

    let err = materialize(&fresh, &other.content, false, "x")
        .await
        .unwrap_err();

    assert!(matches!(err, Error::NotCached(_)), "{err:?}");
    assert!(fresh.leftovers().is_empty());
}

#[tokio::test]
async fn chunks_that_do_not_make_up_the_content_are_refused() {
    for size in [LARGE, 2 << 20] {
        let fx = Fixture::new();
        let real = pseudo_random(size, 21);
        let other = fx.ingest(&pseudo_random(size, 22)).await;
        // A manifest that claims `real` but lists the chunks of `other`.
        let forged = FileManifest {
            content: ContentId::of(&real),
            size: other.size,
            chunks: other.chunks.clone(),
        };
        fx.cache.put_manifest(&forged).unwrap();
        let dest = fx.dest("big.bin");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, b"old").unwrap();

        let err = materialize(&fx, &forged.content, false, "big.bin")
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(_)), "{err:?}");
        assert_eq!(std::fs::read(&dest).unwrap(), b"old");
        assert!(fx.leftovers().is_empty(), "{:?}", fx.leftovers());
        assert!(
            fx.cache.manifest(&forged.content).unwrap().is_none(),
            "the bad manifest is forgotten, to be fetched again"
        );

        fx.cache.put_manifest(&forged).unwrap();
        assert!(fx.cache.read(&forged.content).await.is_err());
        assert!(fx.cache.manifest(&forged.content).unwrap().is_none());
    }
}

#[tokio::test]
async fn a_cached_manifest_of_other_content_is_not_used() {
    let fx = Fixture::new();
    let other = fx.ingest(&pseudo_random(2 << 20, 23)).await;
    let wanted = ContentId::of(b"something else");
    let file = |c: &ContentId| {
        let hex = c.to_string();
        fx.sandbox
            .root()
            .join("cache/manifests")
            .join(&hex[..2])
            .join(hex)
    };
    std::fs::create_dir_all(file(&wanted).parent().unwrap()).unwrap();
    std::fs::copy(file(&other.content), file(&wanted)).unwrap();
    assert!(fx.cache.manifest(&wanted).unwrap().is_none());
    assert!(!file(&wanted).exists());
    assert_eq!(fx.cache.manifest(&other.content).unwrap(), Some(other));
}

#[tokio::test]
async fn corrupt_chunk_keeps_the_existing_file() {
    let fx = Fixture::new();
    let data = pseudo_random(LARGE, 5);
    let manifest = fx.ingest(&data).await;
    std::fs::write(fx.chunk_file(&manifest, 3), b"not a chunk").unwrap();
    let dest = fx.dest("big.bin");
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    std::fs::write(&dest, b"old").unwrap();

    let err = materialize(&fx, &manifest.content, false, "big.bin")
        .await
        .unwrap_err();

    assert!(matches!(err, Error::NotCached(_)), "{err:?}");
    assert_eq!(std::fs::read(&dest).unwrap(), b"old");
    assert!(fx.leftovers().is_empty(), "{:?}", fx.leftovers());
}

#[tokio::test]
async fn replaces_an_existing_read_only_file() {
    for size in [LARGE, 1000] {
        let fx = Fixture::new();
        let data = pseudo_random(size, 9);
        let manifest = fx.ingest(&data).await;
        let dest = fx.dest("locked.bin");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, b"old").unwrap();
        let mut perms = std::fs::metadata(&dest).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&dest, perms).unwrap();
        assert!(is_read_only(&dest).unwrap());

        materialize(&fx, &manifest.content, false, "locked.bin")
            .await
            .unwrap();

        assert!(std::fs::read(&dest).unwrap() == data);
        assert!(!is_read_only(&dest).unwrap());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn sets_the_executable_bit() {
    use std::os::unix::fs::PermissionsExt;
    let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o111;
    for size in [LARGE, 1000] {
        let fx = Fixture::new();
        let manifest = fx.ingest(&pseudo_random(size, 2)).await;

        materialize(&fx, &manifest.content, true, "run.sh")
            .await
            .unwrap();
        materialize(&fx, &manifest.content, false, "data")
            .await
            .unwrap();

        assert_ne!(mode(fx.dest("run.sh")), 0);
        assert_eq!(mode(fx.dest("data")), 0);
    }
}
