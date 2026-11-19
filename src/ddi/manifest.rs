//! The SHA-256 manifest of a mirror of the developer disk image: a text file with a line for each file, `sha256sum`'s
//! own format (`<64 hex digits>  <path>`, the path relative to the root of the repository the files are served from:
//! `PersonalizedImages/Xcode_iOS_DDI_Cryptex/Image.dmg`). It is the only thing that tells scrios what a mirror's files
//! must be, so it is read strictly.

// A manifest is text that someone else wrote: a malformed one is an error, never a panic, so the lints that find the
// panics are denied here (the tests below may use them).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use super::safe_relative;

/// The largest manifest that is read.
pub const MAX_BYTES: usize = 1024 * 1024;
/// The most files a manifest lists.
pub const MAX_ENTRIES: usize = 10_000;

/// The files of a manifest, each with the SHA-256 it must have.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    entries: Vec<(String, String)>,
}

impl Manifest {
    /// Parses `text`: a line is `sha256`, white space, an optional `*` (`sha256sum`'s binary mode), and a path; blank
    /// lines and lines that start with `#` are left out. The path is relative and stays inside the repository (no `..`,
    /// no root, no backslash); a leading `./` is dropped; a path may be listed once.
    ///
    /// # Errors
    ///
    /// A message that names the line that is wrong, or says the manifest is too large or lists too many files.
    pub fn parse(text: &str) -> Result<Manifest, String> {
        if text.len() > MAX_BYTES {
            return Err(format!("the manifest is larger than {MAX_BYTES} bytes"));
        }
        let mut entries: Vec<(String, String)> = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let number = index.saturating_add(1);
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (hash, rest) =
                line.split_once(char::is_whitespace).ok_or_else(|| format!("line {number}: no path after the hash"))?;
            if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(format!("line {number}: {hash:?} is not a SHA-256 (64 hex digits)"));
            }
            let path = rest.trim_start().strip_prefix('*').unwrap_or_else(|| rest.trim_start());
            let path = path.strip_prefix("./").unwrap_or(path);
            let clean = safe_relative(path).map_err(|why| format!("line {number}: {why}"))?;
            let path = clean
                .to_str()
                .ok_or_else(|| format!("line {number}: the path is not text"))?
                .replace(std::path::MAIN_SEPARATOR, "/");
            if entries.iter().any(|(known, _)| *known == path) {
                return Err(format!("line {number}: {path} is listed twice"));
            }
            if entries.len() >= MAX_ENTRIES {
                return Err(format!("the manifest lists more than {MAX_ENTRIES} files"));
            }
            entries.push((path, hash.to_ascii_lowercase()));
        }
        Ok(Manifest { entries })
    }

    /// The SHA-256 (lowercase hex) that `path` (with `/`) must have, if the manifest lists it.
    #[must_use]
    pub fn sha256_of(&self, path: &str) -> Option<&str> {
        self.entries.iter().find(|(known, _)| known == path).map(|(_, hash)| hash.as_str())
    }

    /// How many files the manifest lists.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether it lists none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        reason = "test code: a panic is a failed test, and the inputs are small, fixed values"
    )]
    use super::*;

    const A: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const B: &str = "FEDCBA9876543210FEDCBA9876543210FEDCBA9876543210FEDCBA9876543210";

    #[test]
    fn a_sha256sum_file_is_read_line_by_line() {
        let text = format!(
            "# made by sha256sum\n\n{A}  PersonalizedImages/Xcode_iOS_DDI_Cryptex/Image.dmg\n{B} *./PersonalizedImages/x/BuildManifest.plist\r\n\t{A}\tdir/with space/file\n"
        );
        let manifest = Manifest::parse(&text).unwrap();
        assert_eq!(manifest.len(), 3);
        assert_eq!(manifest.sha256_of("PersonalizedImages/Xcode_iOS_DDI_Cryptex/Image.dmg"), Some(A));
        // The hash is lowercase, and `./` and the binary marker are not part of the path.
        assert_eq!(
            manifest.sha256_of("PersonalizedImages/x/BuildManifest.plist"),
            Some(B.to_ascii_lowercase().as_str())
        );
        assert_eq!(manifest.sha256_of("dir/with space/file"), Some(A));
        assert_eq!(manifest.sha256_of("nope"), None);
        assert!(Manifest::parse("# nothing\n\n").unwrap().is_empty());
    }

    #[test]
    fn a_line_that_is_not_one_is_refused_with_its_number() {
        for (text, want) in [
            (format!("{A}\n"), "line 1: no path"),
            (format!("{A}x  a\n"), "not a SHA-256"),
            (format!("\n{}  a\n", &A[..63]), "line 2: "),
            (format!("{}  a\n", "g".repeat(64)), "not a SHA-256"),
            (format!("{A}  \n"), "line 1"),
            (format!("{A}  a\n{A}  a\n"), "line 2: a is listed twice"),
            (format!("{A}  a\n{A}  ./a\n"), "listed twice"),
        ] {
            let why = Manifest::parse(&text).unwrap_err();
            assert!(why.contains(want), "{text:?}: {why}");
        }
    }

    #[test]
    fn a_path_that_leaves_the_repository_is_refused() {
        for path in ["../x", "a/../../x", "/etc/passwd", "a\\b", "..", "a/./../..", "C:\\x", "\0"] {
            let why = Manifest::parse(&format!("{A}  {path}\n")).unwrap_err();
            assert!(why.starts_with("line 1: "), "{path:?}: {why}");
        }
    }

    #[test]
    fn a_manifest_that_is_too_large_or_lists_too_many_files_is_refused() {
        let why = Manifest::parse(&"#".repeat(MAX_BYTES + 1)).unwrap_err();
        assert!(why.contains("larger than"), "{why}");
        // As many files as there may be fit in the size; one more does not.
        let lines = |count: usize| (0..count).map(|n| format!("{A} f{n}\n")).collect::<Vec<_>>().concat();
        let most = lines(MAX_ENTRIES);
        assert!(most.len() <= MAX_BYTES);
        assert_eq!(Manifest::parse(&most).unwrap().len(), MAX_ENTRIES);
        let why = Manifest::parse(&lines(MAX_ENTRIES + 1)).unwrap_err();
        assert!(why.contains("more than"), "{why}");
    }
}
