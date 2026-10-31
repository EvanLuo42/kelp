//! Describes a change by the file sizes.

wit_bindgen::generate!({ path: "../../wit", world: "differ" });

struct Plugin;

impl Guest for Plugin {
    fn describe(old: Option<FileVersion>, new: Option<FileVersion>) -> String {
        match (old, new) {
            (Some(o), Some(n)) => format!("{}: {} -> {} bytes", n.path, o.content.len(), n.content.len()),
            (None, Some(n)) => format!("{}: new, {} bytes", n.path, n.content.len()),
            (Some(o), None) => format!("{}: deleted", o.path),
            (None, None) => String::new(),
        }
    }
}

export!(Plugin);
