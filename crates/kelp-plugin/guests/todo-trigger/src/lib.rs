//! Rejects submits whose description lacks a ticket, that add files with
//! spaces in their names, or whose files contain `DO NOT SUBMIT`.

wit_bindgen::generate!({ path: "../../wit", world: "trigger" });

struct Plugin;

impl Guest for Plugin {
    fn pre_submit(_stream: String, _author: String, description: String, changes: Vec<Change>) -> Verdict {
        if !description.contains('#') {
            return Verdict::Reject("the description needs a ticket like #123".into());
        }
        for change in &changes {
            if change.action == "add" && change.path.contains(' ') {
                return Verdict::Reject(format!("{} has a space in its name", change.path));
            }
            if let Some(data) = read_file(&change.path)
                && data.windows(13).any(|w| w == b"DO NOT SUBMIT")
            {
                return Verdict::Reject(format!("{} is marked DO NOT SUBMIT", change.path));
            }
        }
        Verdict::Accept
    }
}

export!(Plugin);
