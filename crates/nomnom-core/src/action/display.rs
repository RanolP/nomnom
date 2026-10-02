//! Printing a path the way the user typed it.
//!
//! `Plan` and the journal store canonicalized paths, which on Windows carry the
//! `\\?\` verbatim prefix. That prefix is correct and is what makes long paths
//! work, but it is noise in the one artifact a human is supposed to read before
//! approving a deletion, so it is stripped for display only.

use std::path::Path;

pub fn plain(path: &Path) -> String {
    let text = path.display().to_string();
    match text.strip_prefix(r"\\?\") {
        Some(rest) => rest.to_string(),
        None => text,
    }
}

#[cfg(test)]
mod tests {
    // Catches the verbatim prefix leaking into the plan a human approves on.
    #[test]
    fn strips_only_the_verbatim_prefix() {
        use std::path::Path;
        assert_eq!(super::plain(Path::new(r"\\?\C:\a\b")), r"C:\a\b");
        assert_eq!(super::plain(Path::new(r"C:\a\b")), r"C:\a\b");
    }
}
