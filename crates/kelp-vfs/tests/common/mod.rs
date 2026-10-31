use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::Mutex;

use kelp_vfs::{Hydrator, LowerFile, LowerTree};

/// Serves file content from memory and records which files were hydrated.
#[derive(Default)]
pub struct MemoryHydrator {
    pub content: HashMap<[u8; 32], Vec<u8>>,
    pub hydrated: Mutex<Vec<String>>,
}

impl Hydrator for MemoryHydrator {
    fn hydrate(&self, local: &str, file: &LowerFile, out: &mut dyn Write) -> io::Result<()> {
        self.hydrated.lock().unwrap().push(local.to_owned());
        out.write_all(
            self.content
                .get(&file.content)
                .ok_or(io::ErrorKind::NotFound)?,
        )
    }
}

pub fn lower_file(hydrator: &mut MemoryHydrator, bytes: &[u8]) -> LowerFile {
    let mut id = [0u8; 32];
    id[..8].copy_from_slice(&(hydrator.content.len() as u64 + 1).to_le_bytes());
    hydrator.content.insert(id, bytes.to_vec());
    LowerFile {
        content: id,
        size: bytes.len() as u64,
        executable: false,
        read_only: false,
        mtime: 1_000_000_000,
    }
}

/// A lower tree with `a.txt`, `Content/Hero.png` and `Content/Maps/level.map`.
pub fn sample() -> (LowerTree, MemoryHydrator) {
    let mut hydrator = MemoryHydrator::default();
    let mut lower = LowerTree::new();
    lower.insert("a.txt", lower_file(&mut hydrator, b"alpha"));
    lower.insert("Content/Hero.png", lower_file(&mut hydrator, b"png"));
    lower.insert("Content/Maps/level.map", lower_file(&mut hydrator, b"map"));
    (lower, hydrator)
}
