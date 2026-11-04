//! Showing differences in an external diff tool.
//!
//! The tool is a command line such as `meld {old} {new}`. `{old}` and
//! `{new}` stand for files holding the two versions, and `{path}` for the
//! depot path; a command without `{old}` or `{new}` gets the two files
//! appended. A side that does not exist, such as the old side of an added
//! file, is an empty file. The command runs once per file, without a shell.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};
use kelp_client::FileDiff;

/// The environment variable that names the default diff tool.
pub const ENV: &str = "KELP_DIFF_TOOL";

/// Returns the tool to use: `flag` if given, else [`ENV`] if set and not
/// empty.
pub fn chosen(flag: Option<String>) -> Option<String> {
    flag.or_else(|| std::env::var(ENV).ok())
        .filter(|t| !t.trim().is_empty())
}

/// Runs `tool` on each of `diffs`, one after another.
///
/// # Errors
///
/// Returns an error if the command line is malformed, the temporary files
/// cannot be written, or the tool cannot be started. A tool that exits with
/// an error is reported but does not stop the others, since many diff tools
/// exit with 1 when the files differ.
pub fn run(tool: &str, diffs: &[FileDiff]) -> anyhow::Result<()> {
    let words = split(tool)?;
    if words.is_empty() {
        bail!("the diff tool is empty");
    }
    let scratch = tempfile::tempdir().context("cannot create a directory for the diff tool")?;
    for (i, diff) in diffs.iter().enumerate() {
        let name = diff.path.as_str().rsplit('/').next().unwrap_or("file");
        let old = side(
            scratch.path(),
            &format!("{i}-old"),
            name,
            diff.old.as_deref(),
        )?;
        let new = side(
            scratch.path(),
            &format!("{i}-new"),
            name,
            diff.new.as_deref(),
        )?;
        let args = arguments(&words[1..], &old, &new, diff.path.as_str());
        let status = Command::new(&words[0])
            .args(&args)
            .status()
            .with_context(|| format!("cannot run the diff tool {:?}", words[0]))?;
        if !status.success() && status.code() != Some(1) {
            eprintln!("kelp: the diff tool exited with {status} for {}", diff.path);
        }
    }
    Ok(())
}

/// Writes one side of a comparison to `<dir>/<sub>/<name>`, keeping the
/// file name so tools can tell its type.
fn side(dir: &Path, sub: &str, name: &str, data: Option<&[u8]>) -> anyhow::Result<PathBuf> {
    let dir = dir.join(sub);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(name);
    std::fs::write(&path, data.unwrap_or_default())?;
    Ok(path)
}

/// Substitutes the placeholders in `words`, appending the two files if
/// neither is named.
fn arguments(words: &[String], old: &Path, new: &Path, path: &str) -> Vec<String> {
    let (old, new) = (old.display().to_string(), new.display().to_string());
    let named = words
        .iter()
        .any(|w| w.contains("{old}") || w.contains("{new}"));
    let mut out: Vec<String> = words
        .iter()
        .map(|w| {
            w.replace("{old}", &old)
                .replace("{new}", &new)
                .replace("{path}", path)
        })
        .collect();
    if !named {
        out.push(old);
        out.push(new);
    }
    out
}

/// Splits a command line into words at unquoted whitespace. Single and
/// double quotes group words and are removed; nothing else is special, so
/// Windows paths keep their backslashes.
pub(crate) fn split(line: &str) -> anyhow::Result<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote = None;
    for c in line.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => word.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                in_word = true;
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            (None, c) => {
                word.push(c);
                in_word = true;
            }
        }
    }
    if quote.is_some() {
        bail!("unbalanced quote in the tool command {line:?}");
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_quoted_words_and_fills_placeholders() {
        let words =
            split(r#""C:\Program Files\Meld\meld.exe" --label '{path}' {old} {new}"#).unwrap();
        assert_eq!(
            words,
            [
                r"C:\Program Files\Meld\meld.exe",
                "--label",
                "{path}",
                "{old}",
                "{new}"
            ]
        );
        let args = arguments(
            &words[1..],
            Path::new("a"),
            Path::new("b"),
            "//game/main/x.png",
        );
        assert_eq!(args, ["--label", "//game/main/x.png", "a", "b"]);
        assert_eq!(
            arguments(&["-u".to_owned()], Path::new("a"), Path::new("b"), "//p"),
            ["-u", "a", "b"]
        );
        assert!(split("meld 'open").is_err());
    }
}
