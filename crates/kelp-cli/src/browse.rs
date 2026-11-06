//! `kelp ls` and `kelp tree`, which show what the server has.

use std::path::Path;

use kelp_client::{Connection, Error, Home, Workspace, entry_label, list_depot, render_tree};
use kelp_core::DepotPath;
use kelp_proto::rpc::{DirEntry, DirEntryKind, ListOptions};

use crate::util::{connect, split_revision};

/// Works out what `spec`, as given on the command line, names: a depot path,
/// or `None` for the server's root, and a changelist.
async fn target(
    conn: &Connection,
    cwd: &Path,
    spec: Option<&str>,
) -> anyhow::Result<(Option<DepotPath>, Option<u64>)> {
    let (spec, at) = split_revision(conn, spec.unwrap_or_default()).await?;
    let path = if spec.starts_with("//") {
        match spec.trim_end_matches('/') {
            "" => None,
            path => Some(DepotPath::parse(path)?),
        }
    } else if spec.is_empty() {
        match Workspace::discover(cwd) {
            Ok(ws) => Some(ws.depot_dir_of(cwd)?),
            Err(Error::NoWorkspace) => None,
            Err(e) => return Err(e.into()),
        }
    } else {
        Some(Workspace::discover(cwd)?.depot_dir_of(&cwd.join(spec))?)
    };
    Ok((path, at))
}

pub async fn ls(
    home: &Home,
    cwd: &Path,
    server: &str,
    path: Option<String>,
    long: bool,
    dirs: bool,
) -> anyhow::Result<()> {
    let conn = connect(home, server).await?;
    let (path, at) = target(&conn, cwd, path.as_deref()).await?;
    let options = ListOptions {
        at,
        depth: Some(1),
        dirs_only: dirs,
        details: long,
    };
    for entry in list_depot(&conn, path.as_ref(), options).await? {
        match long {
            true => println!("{}", long_line(&entry)),
            false => println!("{}", entry_label(&entry)),
        }
    }
    Ok(())
}

/// Returns an entry as `ls -l` shows it. Columns that do not apply are empty.
fn long_line(entry: &DirEntry) -> String {
    let (kind, size) = match &entry.kind {
        DirEntryKind::File(file) if file.executable => ('x', file.size.to_string()),
        DirEntryKind::File(file) => ('-', file.size.to_string()),
        DirEntryKind::Dir => ('d', String::new()),
        DirEntryKind::Symlink { .. } => ('l', String::new()),
    };
    let (rev, cl, author, date) = match &entry.last {
        Some(last) => (
            format!("#{}", last.rev),
            format!("@{}", last.cl),
            last.author_name.as_deref().unwrap_or(&last.author),
            format_date(last.submitted_at),
        ),
        None => (String::new(), String::new(), "", String::new()),
    };
    format!(
        "{kind} {rev:<5} {cl:<8} {author:<12} {date:<16} {size:>12}  {}",
        entry_label(entry)
    )
}

/// Formats milliseconds since the Unix epoch as `YYYY-MM-DD HH:MM` in UTC.
fn format_date(millis: u64) -> String {
    let secs = millis / 1000;
    // Howard Hinnant's `civil_from_days`, with days shifted to start in the
    // 400-year era beginning 0000-03-01.
    let days = secs / 86_400 + 719_468;
    let era = days / 146_097;
    let day_of_era = days % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    let (hour, minute) = (secs / 3600 % 24, secs / 60 % 60);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

pub async fn tree(
    home: &Home,
    cwd: &Path,
    server: &str,
    path: Option<String>,
    depth: Option<u32>,
    dirs_only: bool,
) -> anyhow::Result<()> {
    let conn = connect(home, server).await?;
    let (path, at) = target(&conn, cwd, path.as_deref()).await?;
    let options = ListOptions {
        at,
        depth,
        dirs_only,
        details: false,
    };
    let entries = list_depot(&conn, path.as_ref(), options).await?;
    let root = path.as_ref().map_or("//", DepotPath::as_str);
    match at {
        Some(cl) => println!("{root}@{cl}"),
        None => println!("{root}"),
    }
    print!("{}", render_tree(&entries));
    let dirs = entries
        .iter()
        .filter(|e| e.kind == DirEntryKind::Dir)
        .count();
    let plural = |n: usize, one: &str, many: &str| match n {
        1 => format!("1 {one}"),
        n => format!("{n} {many}"),
    };
    let directories = plural(dirs, "directory", "directories");
    match dirs_only {
        true => println!("\n{directories}"),
        false => println!(
            "\n{directories}, {}",
            plural(entries.len() - dirs, "file", "files")
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::format_date;

    #[test]
    fn formats_dates() {
        assert_eq!(format_date(0), "1970-01-01 00:00");
        assert_eq!(format_date(951_782_400_000), "2000-02-29 00:00");
        assert_eq!(format_date(1_793_970_930_000), "2026-11-06 13:15");
    }
}
