//! Issue keys: references to work items such as `PROJ-123`.
//!
//! A key is a project prefix of an uppercase letter followed by uppercase
//! letters, digits or `_`, a `-`, and a number. Kelp keeps no issues itself;
//! keys refer to an external tracker.

/// Returns `key` in canonical form (uppercase) if it is an issue key.
///
/// # Examples
///
/// ```
/// use kelp_core::issue::parse_key;
///
/// assert_eq!(parse_key("proj-42").as_deref(), Some("PROJ-42"));
/// assert_eq!(parse_key("PROJ-"), None);
/// assert_eq!(parse_key("42-PROJ"), None);
/// ```
pub fn parse_key(key: &str) -> Option<String> {
    let key = key.trim().to_ascii_uppercase();
    let (prefix, number) = key.split_once('-')?;
    let prefix_ok = prefix.len() >= 2
        && prefix.starts_with(|c: char| c.is_ascii_uppercase())
        && prefix
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    let number_ok =
        !number.is_empty() && number.len() <= 18 && number.bytes().all(|b| b.is_ascii_digit());
    (prefix_ok && number_ok).then_some(key)
}

/// Finds the issue keys mentioned in `text`, in order and without
/// duplicates. With `prefixes`, only keys of those projects count;
/// otherwise any key does.
///
/// A key must be a whole word and written in uppercase, so that ordinary
/// words with hyphens are not mistaken for keys.
///
/// # Examples
///
/// ```
/// use kelp_core::issue::find_keys;
///
/// let text = "Fix shadows (GFX-12, see also ART-3); not utf-8 or GFX-12x";
/// assert_eq!(find_keys(text, None), ["GFX-12", "ART-3"]);
/// assert_eq!(find_keys(text, Some(&["ART".to_owned()])), ["ART-3"]);
/// ```
pub fn find_keys(text: &str, prefixes: Option<&[String]>) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    for token in text.split(|c: char| !word(c)) {
        let token = token.trim_matches('-');
        if token.chars().any(|c| c.is_ascii_lowercase()) {
            continue;
        }
        let Some(key) = parse_key(token) else {
            continue;
        };
        let prefix = key.split_once('-').map_or("", |(p, _)| p);
        let wanted = prefixes.is_none_or(|ps| ps.iter().any(|p| p.eq_ignore_ascii_case(prefix)));
        if wanted && !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys
}
