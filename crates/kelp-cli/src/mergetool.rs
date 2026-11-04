//! Resolving conflicts in an external merge tool.
//!
//! The tool is a command line such as `meld {yours} {base} {theirs} --output
//! {merged}`. `{base}`, `{yours}` and `{theirs}` stand for files holding the
//! three versions, `{merged}` for the file the tool writes the result to, and
//! `{path}` for the depot path. A command without any of the four files gets
//! `{base} {yours} {theirs} {merged}` appended. A version that does not exist
//! is an empty file. The command runs once per file, without a shell, and
//! succeeds if it exits with 0.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};
use kelp_client::MergeInput;

/// The environment variable that names the default merge tool.
pub const ENV: &str = "KELP_MERGE_TOOL";

/// Returns the tool to use: `flag` if given, else [`ENV`] if set and not
/// empty.
pub fn chosen(flag: Option<String>) -> Option<String> {
    flag.or_else(|| std::env::var(ENV).ok())
        .filter(|t| !t.trim().is_empty())
}

/// What came of running the tool on one file.
#[derive(Debug, PartialEq, Eq)]
pub enum Merged {
    /// The result was written to the workspace file.
    Done,
    /// The file is left as it was, for the stated reason.
    Skipped(String),
}

/// Runs `tool` on `input`, and writes what it leaves in `{merged}` to the
/// workspace file if it succeeds.
///
/// `current` is what the workspace file holds now, which the tool starts
/// from: the automatic merge with conflict markers, or the file as it is.
///
/// # Errors
///
/// Returns an error if the command line is malformed, the temporary files
/// cannot be written, the tool cannot be started, or the result cannot be
/// written to the workspace.
pub fn run(tool: &str, input: &MergeInput, current: &[u8]) -> anyhow::Result<Merged> {
    let words = crate::difftool::split(tool)?;
    if words.is_empty() {
        bail!("the merge tool is empty");
    }
    let (Some(yours), Some(theirs)) = (&input.yours, &input.theirs) else {
        return Ok(Merged::Skipped(
            "one side deletes it; use `--accept yours` or `--accept theirs`".into(),
        ));
    };
    let name = input.path.as_str().rsplit('/').next().unwrap_or("file");
    let scratch = tempfile::tempdir().context("cannot create a directory for the merge tool")?;
    let base = side(scratch.path(), "base", name, input.base.as_deref())?;
    let yours_file = side(scratch.path(), "yours", name, Some(yours))?;
    let theirs_file = side(scratch.path(), "theirs", name, Some(theirs))?;
    let merged = side(scratch.path(), "merged", name, Some(current))?;
    let args = arguments(
        &words[1..],
        &Files {
            base: &base,
            yours: &yours_file,
            theirs: &theirs_file,
            merged: &merged,
        },
        input.path.as_str(),
    );
    let status = Command::new(&words[0])
        .args(&args)
        .status()
        .with_context(|| format!("cannot run the merge tool {:?}", words[0]))?;
    if !status.success() {
        return Ok(Merged::Skipped(format!(
            "the merge tool exited with {status}"
        )));
    }
    let result = std::fs::read(&merged).context("cannot read the merge tool's result")?;
    let versions = [input.base.as_deref(), Some(yours), Some(theirs)];
    if has_markers(&result) && !versions.iter().flatten().any(|v| has_markers(v)) {
        return Ok(Merged::Skipped(
            "the result still has conflict markers; fix them and save, then run the tool again"
                .into(),
        ));
    }
    std::fs::write(&input.local, result)
        .with_context(|| format!("cannot write {}", input.local.display()))?;
    Ok(Merged::Done)
}

/// The files a merge tool is given.
struct Files<'a> {
    base: &'a Path,
    yours: &'a Path,
    theirs: &'a Path,
    merged: &'a Path,
}

/// Writes one file for the tool to `<dir>/<sub>/<name>`, keeping the file
/// name so tools can tell its type.
fn side(dir: &Path, sub: &str, name: &str, data: Option<&[u8]>) -> anyhow::Result<PathBuf> {
    let dir = dir.join(sub);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(name);
    std::fs::write(&path, data.unwrap_or_default())?;
    Ok(path)
}

/// Substitutes the placeholders in `words`, appending the four files if
/// none is named.
fn arguments(words: &[String], files: &Files, path: &str) -> Vec<String> {
    let show = |p: &Path| p.display().to_string();
    let named = words.iter().any(|w| {
        ["{base}", "{yours}", "{theirs}", "{merged}"]
            .iter()
            .any(|p| w.contains(p))
    });
    let mut out: Vec<String> = words
        .iter()
        .map(|w| {
            w.replace("{base}", &show(files.base))
                .replace("{yours}", &show(files.yours))
                .replace("{theirs}", &show(files.theirs))
                .replace("{merged}", &show(files.merged))
                .replace("{path}", path)
        })
        .collect();
    if !named {
        out.extend([files.base, files.yours, files.theirs, files.merged].map(show));
    }
    out
}

/// Whether `data` has a line that starts a conflict, as `diffy` and other
/// merge programs write them.
fn has_markers(data: &[u8]) -> bool {
    data.split(|&b| b == b'\n').any(|line| {
        line.starts_with(b"<<<<<<<") && (line.len() == 7 || line[7] == b' ' || line[7] == b'\r')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_placeholders_or_appends_the_files() {
        let files = Files {
            base: Path::new("b"),
            yours: Path::new("y"),
            theirs: Path::new("t"),
            merged: Path::new("m"),
        };
        // The words after the program's name, as `run` passes them.
        let words = |line: &str| crate::difftool::split(line).unwrap()[1..].to_vec();
        assert_eq!(
            arguments(
                &words("meld {yours} {base} {theirs} --output {merged} --label {path}"),
                &files,
                "//g/x"
            ),
            ["y", "b", "t", "--output", "m", "--label", "//g/x"]
        );
        assert_eq!(
            arguments(&words("tool -x"), &files, "//g/x"),
            ["-x", "b", "y", "t", "m"]
        );
    }

    #[test]
    fn finds_conflict_markers() {
        assert!(has_markers(
            b"a\n<<<<<<< ours\nb\n=======\nc\n>>>>>>> theirs\n"
        ));
        assert!(has_markers(b"<<<<<<<\r\nx"));
        assert!(!has_markers(b"a\n<<<<<<<<<<\nb"));
        assert!(!has_markers(b"plain\ntext\n"));
    }
}
