//! Merges line-set files, where each line is an independent entry: lines
//! added or removed on either side are applied to the base.

wit_bindgen::generate!({ path: "../../wit", world: "merger" });

struct Plugin;

fn lines(v: &FileVersion) -> Vec<String> {
    String::from_utf8_lossy(&v.content)
        .lines()
        .map(str::to_owned)
        .collect()
}

impl Guest for Plugin {
    fn merge(base: FileVersion, ours: FileVersion, theirs: FileVersion) -> MergeResult {
        let (base, ours, theirs) = (lines(&base), lines(&ours), lines(&theirs));
        if ours.iter().chain(&theirs).any(|l| l.starts_with("!conflict")) {
            return MergeResult::Conflict("a side asks for a conflict".into());
        }
        let removed = |l: &String| {
            base.contains(l) && (!ours.contains(l) || !theirs.contains(l))
        };
        let mut merged: Vec<String> = base.iter().filter(|l| !removed(l)).cloned().collect();
        for l in ours.iter().chain(&theirs) {
            if !base.contains(l) && !merged.contains(l) {
                merged.push(l.clone());
            }
        }
        merged.sort();
        let mut out = merged.join("\n");
        out.push('\n');
        MergeResult::Merged(out.into_bytes())
    }
}

export!(Plugin);
