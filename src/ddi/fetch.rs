//! Downloading the files of a developer disk image, checking every one against its size and SHA-256 before it is
//! used, and keeping the checked files in the cache.
//!
//! What to download is a [`Plan`]: a list of files, each with the URL it is downloaded from and the SHA-256 (and, for
//! the pinned image, the size) it must have. The pinned plan ([`Plan::pinned`]) is the table in [`super::pin`]; a mirror's
//! ([`Plan::mirror`]) is made from a manifest of SHA-256 values that the user gave ([`Mirror`]). Nothing that is
//! downloaded is trusted until it matches: a file whose size or hash is wrong is an error, and nothing of it is written.
//!
//! The cache keeps a folder for each build: `<cache>/<build>/<folder>/` (`27B52/Xcode_iOS_DDI_Cryptex/`), a file at a
//! time, written to a temporary name and renamed into place, so that a download that stops half way is never mistaken for
//! a file. The files are checked again every time they are used (a hash of some tens of MB takes a moment): a file that
//! was damaged or changed since is downloaded again; a cache that is all good is used without any network.

// What is downloaded is someone else's bytes, and what is checked is their size and hash: a failure is an error, never a
// panic, so the lints that find the panics are denied here (the tests below may use them).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::fmt;
use std::future::Future;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};

use super::http::{self, HttpError, Limits, Url};
use super::manifest::Manifest;
use super::{DdiFolder, Variant, pin, safe_relative, sha256_hex};

/// The largest file that is downloaded (the image is some tens of MB).
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// A boxed future (what a trait object's async method returns).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Where bytes come from: an https client (the one in the program, [`Https`]), or something else that a program
/// supplies (a client with a proxy, a fake in a test). It is given the URL, the most bytes it may return, and a callback
/// for how many bytes it has so far.
pub trait Fetch: Send + Sync + fmt::Debug {
    /// The body of `url`, of at most `max_bytes`.
    fn get<'a>(
        &'a self,
        url: &'a Url,
        max_bytes: u64,
        progress: &'a mut (dyn FnMut(u64) + Send),
    ) -> BoxFuture<'a, Result<Vec<u8>, HttpError>>;
}

/// The program's own https client ([`http::get`]): https only, redirects only to the same host or GitHub's raw hosts,
/// a size cap and timeouts.
#[derive(Debug, Clone, Copy, Default)]
pub struct Https;

impl Fetch for Https {
    fn get<'a>(
        &'a self,
        url: &'a Url,
        max_bytes: u64,
        progress: &'a mut (dyn FnMut(u64) + Send),
    ) -> BoxFuture<'a, Result<Vec<u8>, HttpError>> {
        Box::pin(async move { http::get(url, &Limits::for_file(max_bytes), progress).await })
    }
}

/// What a download says as it goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchEvent {
    /// Files are about to be downloaded: from where, for which Xcode build (if it is known), and how many bytes in all
    /// (if that is known).
    Downloading { source: String, build: Option<String>, bytes: Option<u64> },
    /// The download is this far (percent, in tens).
    Progress { percent: u8 },
    /// Every file that was downloaded has the size and SHA-256 that was expected, and is in the cache.
    Verified,
}

/// The size for a line: `18 MB`, `428 bytes`.
fn human(bytes: u64) -> String {
    if bytes >= 1_000_000 {
        format!("{} MB", bytes.saturating_add(500_000).checked_div(1_000_000).unwrap_or(0))
    } else if bytes >= 1_000 {
        format!("{} KB", bytes.saturating_add(500).checked_div(1_000).unwrap_or(0))
    } else {
        format!("{bytes} bytes")
    }
}

impl FetchEvent {
    /// The plain line to show for the event.
    #[must_use]
    pub fn line(&self) -> String {
        match self {
            FetchEvent::Downloading { source, build, bytes } => {
                let of = build.as_ref().map_or_else(String::new, |build| format!(" (Xcode {build})"));
                let size = bytes.map_or_else(String::new, |bytes| format!(": {}", human(bytes)));
                format!("Downloading the developer disk image{of} from {source}{size}")
            }
            FetchEvent::Progress { percent } => format!("Downloading: {percent}%"),
            FetchEvent::Verified => "Verified: every file has the size and SHA-256 that were expected".to_string(),
        }
    }
}

// ---- what to download ---------------------------------------------------------------------------

/// A file to download: its name in the folder, where from, and what it must be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    /// The name in the folder (`Image.dmg`): one path component.
    pub name: String,
    /// Where it is downloaded from.
    pub url: Url,
    /// The SHA-256 it must have (lowercase hex).
    pub sha256: String,
    /// The size it must have, if it is known (the pinned image's is; a mirror's manifest has none).
    pub size: Option<u64>,
}

/// The files of one variant of the image, and where they come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub variant: Variant,
    /// Where the files come from, for a line (`github.com/doronz88/DeveloperDiskImage`).
    pub source: String,
    /// The Xcode build the image is of, if it is known beforehand (the pinned image's is).
    pub build: Option<String>,
    /// The folder of the cache the files are kept in (the build, or `mirror-<hash>`).
    pub subdir: String,
    pub files: Vec<PlannedFile>,
}

impl Plan {
    /// The plan for the pinned image of `variant` ([`super::pin`]).
    ///
    /// # Errors
    ///
    /// A message if a URL of the pin is not one that is downloaded from (the tests make sure it is).
    pub fn pinned(variant: Variant) -> Result<Plan, String> {
        let base =
            format!("https://{}/{}/{}/{}/{}", pin::HOST, pin::REPO, pin::COMMIT, pin::IMAGES_DIR, variant.folder());
        let files = pin::files(variant)
            .iter()
            .map(|file| {
                Ok(PlannedFile {
                    name: file.name.to_string(),
                    url: Url::parse(&format!("{base}/{}", file.name)).map_err(|e| format!("{}: {e}", file.name))?,
                    sha256: file.sha256.to_string(),
                    size: Some(file.size),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(Plan {
            variant,
            source: pin::SOURCE.to_string(),
            build: Some(pin::BUILD.to_string()),
            subdir: pin::BUILD.to_string(),
            files,
        })
    }

    /// The plan for `variant` from a mirror: the files the pinned plan lists, at the mirror's URLs, with the SHA-256
    /// values of its manifest.
    ///
    /// # Errors
    ///
    /// A message if the manifest has no entry for a file of the variant.
    pub fn mirror(mirror: &Mirror, variant: Variant) -> Result<Plan, String> {
        let mut files = Vec::new();
        for file in pin::files(variant) {
            let in_repo = format!("{}/{}/{}", pin::IMAGES_DIR, variant.folder(), file.name);
            let sha256 = mirror
                .manifest
                .sha256_of(&in_repo)
                .or_else(|| mirror.manifest.sha256_of(&format!("{}/{}", variant.folder(), file.name)))
                .ok_or_else(|| {
                    format!(
                        "the SHA-256 manifest has no entry for {in_repo}: a mirror's files are not used without one"
                    )
                })?;
            let url = Url::parse(&format!("{}/{}/{in_repo}", mirror.base.as_text(), mirror.git_ref))
                .map_err(|e| format!("{in_repo}: {e}"))?;
            files.push(PlannedFile { name: file.name.to_string(), url, sha256: sha256.to_string(), size: None });
        }
        Ok(Plan { variant, source: mirror.label.clone(), build: None, subdir: mirror.subdir.clone(), files })
    }

    /// The folder of the cache (`cache` is [`super::cache_dir`]) where the files are kept.
    #[must_use]
    pub fn dir_in(&self, cache: &Path) -> PathBuf {
        cache.join(&self.subdir).join(self.variant.folder())
    }

    /// How many bytes the files are in all, if each size is known.
    #[must_use]
    pub fn total_bytes(&self) -> Option<u64> {
        self.files.iter().try_fold(0_u64, |sum, file| file.size.map(|size| sum.saturating_add(size)))
    }

    /// The folder in the cache, if every file of it is there and has the size and SHA-256 it must have (the files are
    /// read and hashed), and it is a folder of the variant.
    #[must_use]
    pub fn cached(&self, cache: &Path) -> Option<DdiFolder> {
        let dir = self.dir_in(cache);
        if !self.files.iter().all(|file| matches!(state_of(&dir, file), State::Good)) {
            return None;
        }
        DdiFolder::open(&dir).ok().filter(|folder| folder.check(self.variant).is_ok())
    }

    /// The folder in the cache, downloading what is not there (or is not what it must be) with `fetcher` and checking
    /// it: every file is checked against its size and SHA-256 before it is written, and a file that fails is an error
    /// that leaves nothing of it. A cache that is all good is used without any download (and `events` hears nothing).
    ///
    /// # Errors
    ///
    /// A message that says which file failed and why: no connection, a server that is not the one that was asked for, a
    /// file that is too large, the wrong size or the wrong SHA-256; or that the cache cannot be written.
    pub async fn fetch(
        &self,
        cache: &Path,
        fetcher: &dyn Fetch,
        events: &mut (dyn FnMut(FetchEvent) + Send),
    ) -> Result<DdiFolder, String> {
        let dir = self.dir_in(cache);
        let todo: Vec<&PlannedFile> =
            self.files.iter().filter(|file| !matches!(state_of(&dir, file), State::Good)).collect();
        if !todo.is_empty() {
            // The cache has to be writable, and it is better to know before the network is asked.
            std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
            let outcome = self.download(&dir, &todo, fetcher, events).await;
            if let Err(why) = outcome {
                // Nothing is left of a failed download but what was already good: an empty folder is not.
                let _ = std::fs::remove_dir(&dir);
                if let Some(build) = dir.parent() {
                    let _ = std::fs::remove_dir(build);
                }
                return Err(why);
            }
        }
        let folder =
            DdiFolder::open(&dir).map_err(|why| format!("the downloaded files are no developer disk image: {why}"))?;
        folder.check(self.variant).map_err(|why| format!("the downloaded files are no use: {why}"))?;
        Ok(folder)
    }

    async fn download(
        &self,
        dir: &Path,
        todo: &[&PlannedFile],
        fetcher: &dyn Fetch,
        events: &mut (dyn FnMut(FetchEvent) + Send),
    ) -> Result<(), String> {
        let total = todo.iter().try_fold(0_u64, |sum, file| file.size.map(|size| sum.saturating_add(size)));
        events(FetchEvent::Downloading { source: self.source.clone(), build: self.build.clone(), bytes: total });
        let mut meter = Meter { total, last: None };
        if let Some(percent) = meter.at(0) {
            events(FetchEvent::Progress { percent });
        }
        let mut done_before = 0_u64;
        for file in todo {
            let cap = file.size.map_or(MAX_FILE_BYTES, |size| size.min(MAX_FILE_BYTES));
            let bytes = {
                let mut on_bytes = |received: u64| {
                    if let Some(percent) = meter.at(done_before.saturating_add(received)) {
                        events(FetchEvent::Progress { percent });
                    }
                };
                fetcher
                    .get(&file.url, cap, &mut on_bytes)
                    .await
                    .map_err(|e| format!("cannot download {} from {}: {e}", file.name, file.url.host()))?
            };
            verify(file, &bytes)?;
            write_atomic(dir, &file.name, &bytes)?;
            done_before = done_before.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        }
        events(FetchEvent::Verified);
        Ok(())
    }
}

/// Tells when a download passes the next tenth of its bytes.
struct Meter {
    total: Option<u64>,
    last: Option<u8>,
}

impl Meter {
    /// The percent (a multiple of ten) to say now that `done` bytes are in, if it is one that was not said yet.
    fn at(&mut self, done: u64) -> Option<u8> {
        let total = self.total.filter(|total| *total > 0)?;
        let percent = done.min(total).saturating_mul(100).checked_div(total).unwrap_or(0);
        let tenth = u8::try_from(percent.checked_div(10).unwrap_or(0)).unwrap_or(10);
        if self.last == Some(tenth) {
            return None;
        }
        self.last = Some(tenth);
        Some(tenth.saturating_mul(10))
    }
}

/// Whether `bytes` are what `file` must be: the size, then the SHA-256.
fn verify(file: &PlannedFile, bytes: &[u8]) -> Result<(), String> {
    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if let Some(size) = file.size.filter(|size| *size != len) {
        return Err(format!("{} is {len} bytes, not the {size} that were expected: not used", file.name));
    }
    let got = sha256_hex(bytes);
    if got != file.sha256 {
        return Err(format!(
            "{} has the SHA-256 {got}, not the {} that was expected: not used",
            file.name, file.sha256
        ));
    }
    Ok(())
}

/// What the cache has of a file.
enum State {
    Good,
    Missing,
    Bad,
}

fn state_of(dir: &Path, file: &PlannedFile) -> State {
    let path = dir.join(&file.name);
    let Ok(meta) = std::fs::metadata(&path) else { return State::Missing };
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES || file.size.is_some_and(|size| size != meta.len()) {
        return State::Bad;
    }
    match std::fs::read(&path) {
        Ok(bytes) if sha256_hex(&bytes) == file.sha256 => State::Good,
        _ => State::Bad,
    }
}

static PARTIAL: AtomicU32 = AtomicU32::new(0);

/// Writes `bytes` as `dir/name` so that the file is never there half written: to a temporary name in the same folder,
/// flushed to the disk, then renamed.
fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), String> {
    let plain = safe_relative(name).map_err(|why| format!("{name}: {why}"))?;
    let temp = dir.join(format!(".{name}.partial-{}-{}", std::process::id(), PARTIAL.fetch_add(1, Ordering::Relaxed)));
    let written = std::fs::File::create(&temp).and_then(|mut file| {
        file.write_all(bytes)?;
        file.sync_all()
    });
    if let Err(e) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(format!("cannot write {}: {e}", temp.display()));
    }
    let dest = dir.join(plain);
    std::fs::rename(&temp, &dest).map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        format!("cannot move {} into place: {e}", dest.display())
    })
}

// ---- a mirror -----------------------------------------------------------------------------------

/// A mirror of the `DeveloperDiskImage` repository: its URL, the revision to read, and the manifest of SHA-256 values its
/// files must have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mirror {
    /// The URL that files are under: `<base>/<ref>/PersonalizedImages/<folder>/<name>`.
    base: Url,
    git_ref: String,
    manifest: Manifest,
    /// For a line: the mirror's host and path.
    label: String,
    /// The folder of the cache for this mirror and revision.
    subdir: String,
}

impl Mirror {
    /// A mirror at `repo`, read at `git_ref`, whose files must match `manifest`. A `https://github.com/<owner>/<repo>`
    /// URL is the repository's raw file service (`raw.githubusercontent.com/<owner>/<repo>`); any other `https` URL is
    /// the place that `<ref>/PersonalizedImages/...` is under.
    ///
    /// # Errors
    ///
    /// A message if the URL is not an https URL of that form, or the revision is not a plain name (a commit, a tag).
    pub fn new(repo: &str, git_ref: &str, manifest: Manifest) -> Result<Mirror, String> {
        let url = Url::parse(repo.trim()).map_err(|e| format!("--ddi-repo: {e}"))?;
        if !url.is_secure() {
            return Err("--ddi-repo must be an https URL".into());
        }
        if url.path().contains('?') {
            return Err("--ddi-repo has a query, which a repository URL does not".into());
        }
        let path = url.path().trim_end_matches('/');
        let git_ref = git_ref.trim();
        let ref_ok = !git_ref.is_empty()
            && git_ref.len() <= 100
            && !git_ref.starts_with(['.', '-'])
            && !git_ref.contains("..")
            && git_ref.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        if !ref_ok {
            return Err("--ddi-ref must be one commit (or tag): letters, digits, `.`, `_` and `-`".into());
        }
        let (base, label) = if url.host() == "github.com" {
            let name = path.strip_suffix(".git").unwrap_or(path);
            let mut parts = name.trim_start_matches('/').split('/');
            let (Some(owner), Some(repo), None) = (parts.next(), parts.next(), parts.next()) else {
                return Err("--ddi-repo on github.com must be https://github.com/<owner>/<repo>".into());
            };
            if owner.is_empty() || repo.is_empty() {
                return Err("--ddi-repo on github.com must be https://github.com/<owner>/<repo>".into());
            }
            let base =
                Url::parse(&format!("https://{}/{owner}/{repo}", pin::HOST)).map_err(|e| format!("--ddi-repo: {e}"))?;
            (base, format!("github.com/{owner}/{repo}"))
        } else {
            (
                Url::parse(&format!("https://{}{path}", url.authority())).map_err(|e| format!("--ddi-repo: {e}"))?,
                format!("{}{path}", url.authority()),
            )
        };
        let subdir = format!(
            "mirror-{}",
            sha256_hex(format!("{}\n{git_ref}", base.as_text()).as_bytes()).get(..16).unwrap_or_default()
        );
        Ok(Mirror { base, git_ref: git_ref.to_string(), manifest, label, subdir })
    }

    /// The mirror's host and path, for a line.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
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
    use crate::testdir::TempDir;
    use crate::testnet::FakeFetch;

    /// A plan of the cryptex folder with small content: the manifest (a real one, from `testddi`) and the image.
    fn small_plan() -> (Plan, Vec<(String, Vec<u8>)>) {
        let dir = TempDir::new("fetch-content");
        crate::testddi::cryptex_folder(&dir);
        let mut contents = Vec::new();
        let mut files = Vec::new();
        for name in [
            "BuildManifest.plist",
            "Image.dmg",
            "Image.dmg.trustcache",
            "Image.dmg.cryptex_info",
            "Image.dmg.root_hash",
        ] {
            let bytes = std::fs::read(dir.path().join(name)).unwrap();
            files.push(PlannedFile {
                name: name.to_string(),
                url: Url::parse(&format!(
                    "https://raw.githubusercontent.com/o/r/abc/PersonalizedImages/Xcode_iOS_DDI_Cryptex/{name}"
                ))
                .unwrap(),
                sha256: sha256_hex(&bytes),
                size: Some(bytes.len() as u64),
            });
            contents.push((files.last().unwrap().url.as_text(), bytes));
        }
        let plan = Plan {
            variant: Variant::Cryptex,
            source: "github.com/o/r".into(),
            build: Some("27B52".into()),
            subdir: "27B52".into(),
            files,
        };
        (plan, contents)
    }

    fn serve(contents: &[(String, Vec<u8>)]) -> FakeFetch {
        let fake = FakeFetch::default();
        for (url, bytes) in contents {
            fake.answer(url, Ok(bytes.clone()));
        }
        fake
    }

    async fn run(plan: &Plan, cache: &Path, fetcher: &FakeFetch) -> (Result<DdiFolder, String>, Vec<FetchEvent>) {
        let mut events = Vec::new();
        let result = plan.fetch(cache, fetcher, &mut |event| events.push(event)).await;
        (result, events)
    }

    #[tokio::test]
    async fn the_files_are_downloaded_checked_and_kept_under_the_build() {
        let (plan, contents) = small_plan();
        let cache = TempDir::new("fetch-cache");
        let fake = serve(&contents);
        let (folder, events) = run(&plan, cache.path(), &fake).await;
        let folder = folder.unwrap();
        assert_eq!(folder.root(), cache.path().join("27B52").join("Xcode_iOS_DDI_Cryptex"));
        assert_eq!(fake.asked().len(), 5);
        // The files are there with their content, and nothing else (no partial file).
        for (url, bytes) in &contents {
            let name = url.rsplit('/').next().unwrap();
            assert_eq!(std::fs::read(folder.root().join(name)).unwrap(), *bytes, "{name}");
        }
        assert_eq!(std::fs::read_dir(folder.root()).unwrap().count(), 5);
        // What was said: where from, progress in tens up to 100, then that it is verified.
        assert!(
            matches!(&events[0], FetchEvent::Downloading { source, build, bytes: Some(_) } if source == "github.com/o/r" && build.as_deref() == Some("27B52")),
            "{events:?}"
        );
        let percents: Vec<u8> = events
            .iter()
            .filter_map(|e| if let FetchEvent::Progress { percent } = e { Some(*percent) } else { None })
            .collect();
        assert_eq!(percents.first(), Some(&0));
        assert_eq!(percents.last(), Some(&100));
        assert!(percents.windows(2).all(|w| w[0] < w[1] && w[0] % 10 == 0), "{percents:?}");
        assert_eq!(events.last(), Some(&FetchEvent::Verified));
    }

    #[tokio::test]
    async fn a_cache_that_is_good_is_used_without_downloading_and_says_nothing() {
        let (plan, contents) = small_plan();
        let cache = TempDir::new("fetch-cache-hit");
        let fake = serve(&contents);
        run(&plan, cache.path(), &fake).await.0.unwrap();
        assert!(plan.cached(cache.path()).is_some());
        let again = FakeFetch::default();
        let (folder, events) = run(&plan, cache.path(), &again).await;
        assert!(folder.is_ok());
        assert!(again.asked().is_empty(), "{:?}", again.asked());
        assert!(events.is_empty(), "{events:?}");
    }

    #[tokio::test]
    async fn a_file_of_the_cache_that_is_damaged_or_missing_is_downloaded_again_and_the_others_are_not() {
        let (plan, contents) = small_plan();
        let cache = TempDir::new("fetch-cache-damaged");
        let fake = serve(&contents);
        let folder = run(&plan, cache.path(), &fake).await.0.unwrap();
        // Same size, other content (a flipped bit); and one file gone.
        let image = folder.root().join("Image.dmg");
        let mut bytes = std::fs::read(&image).unwrap();
        bytes[0] ^= 1;
        std::fs::write(&image, &bytes).unwrap();
        std::fs::remove_file(folder.root().join("Image.dmg.root_hash")).unwrap();
        assert!(plan.cached(cache.path()).is_none());
        let again = serve(&contents);
        let (result, events) = run(&plan, cache.path(), &again).await;
        assert!(result.is_ok(), "{result:?}");
        let mut asked = again.asked();
        asked.sort();
        assert_eq!(asked.len(), 2, "{asked:?}");
        assert!(asked[0].ends_with("/Image.dmg") && asked[1].ends_with("/Image.dmg.root_hash"), "{asked:?}");
        assert_eq!(std::fs::read(&image).unwrap(), contents[1].1);
        assert_eq!(events.last(), Some(&FetchEvent::Verified));
        // A file of the wrong size is as damaged.
        std::fs::write(&image, b"short").unwrap();
        assert!(plan.cached(cache.path()).is_none());
    }

    #[tokio::test]
    async fn a_download_with_the_wrong_hash_or_size_is_refused_and_nothing_is_cached() {
        let (plan, mut contents) = small_plan();
        let cache = TempDir::new("fetch-cache-wrong");
        // The right size, the wrong content.
        let image = contents.iter_mut().find(|(url, _)| url.ends_with("/Image.dmg")).unwrap();
        image.1[0] ^= 1;
        let fake = serve(&contents);
        let (result, events) = run(&plan, cache.path(), &fake).await;
        let why = result.unwrap_err();
        assert!(why.contains("Image.dmg has the SHA-256") && why.contains("not used"), "{why}");
        assert!(!events.contains(&FetchEvent::Verified), "{events:?}");
        // Nothing of the failed file, and no folder left behind; the file before it in the plan was good and is kept.
        let dir = plan.dir_in(cache.path());
        assert!(!dir.join("Image.dmg").exists());
        for entry in std::fs::read_dir(&dir).into_iter().flatten() {
            let name = entry.unwrap().file_name().to_string_lossy().to_string();
            assert!(!name.contains("partial"), "{name}");
        }
        // The wrong size.
        let (plan, mut contents) = small_plan();
        let image = contents.iter_mut().find(|(url, _)| url.ends_with("/Image.dmg")).unwrap();
        image.1.pop();
        let other = TempDir::new("fetch-cache-size");
        let (result, _) = run(&plan, other.path(), &serve(&contents)).await;
        let why = result.unwrap_err();
        assert!(why.contains("Image.dmg is") && why.contains("bytes, not the"), "{why}");
        assert!(!plan.dir_in(other.path()).join("Image.dmg").exists());
    }

    #[tokio::test]
    async fn a_file_larger_than_it_must_be_is_refused_by_the_cap_and_a_failed_first_file_leaves_no_folder() {
        let (plan, mut contents) = small_plan();
        // The manifest is the first file of the plan: too large for its size.
        contents[0].1.extend_from_slice(b"more");
        let cache = TempDir::new("fetch-cache-large");
        let (result, _) = run(&plan, cache.path(), &serve(&contents)).await;
        let why = result.unwrap_err();
        assert!(why.contains("BuildManifest.plist") && why.contains("larger than"), "{why}");
        assert!(!cache.path().join("27B52").exists(), "an empty folder was left");
        // A server error names the file and the host only.
        let (result, _) = run(&plan, cache.path(), &FakeFetch::default()).await;
        let why = result.unwrap_err();
        assert!(
            why.contains("cannot download BuildManifest.plist from raw.githubusercontent.com") && why.contains("404"),
            "{why}"
        );
        assert!(!why.contains("PersonalizedImages"), "the URL's path is not shown: {why}");
    }

    #[test]
    fn the_pinned_plans_are_the_tables_at_the_pinned_commit() {
        let cryptex = Plan::pinned(Variant::Cryptex).unwrap();
        assert_eq!(cryptex.files.len(), 5);
        assert_eq!(cryptex.build.as_deref(), Some(pin::BUILD));
        assert_eq!(cryptex.subdir, pin::BUILD);
        assert_eq!(cryptex.source, "github.com/doronz88/DeveloperDiskImage");
        let image = cryptex.files.iter().find(|f| f.name == "Image.dmg").unwrap();
        assert_eq!(
            image.url.as_text(),
            format!(
                "https://raw.githubusercontent.com/doronz88/DeveloperDiskImage/{}/PersonalizedImages/Xcode_iOS_DDI_Cryptex/Image.dmg",
                pin::COMMIT
            )
        );
        assert_eq!(cryptex.total_bytes(), Some(pin::CRYPTEX_FILES.iter().map(|f| f.size).sum()));
        assert_eq!(Plan::pinned(Variant::Personalized).unwrap().files.len(), 3);
        assert_eq!(cryptex.dir_in(Path::new("/c")), Path::new("/c").join(pin::BUILD).join("Xcode_iOS_DDI_Cryptex"));
        // Every file of both variants is downloaded from the host that serves the pinned commit, over https.
        for variant in [Variant::Cryptex, Variant::Personalized] {
            for file in Plan::pinned(variant).unwrap().files {
                assert!(
                    file.url.is_secure() && file.url.host() == pin::HOST && file.url.port() == 443,
                    "{}",
                    file.name
                );
            }
        }
    }

    #[test]
    fn a_mirror_has_the_files_of_its_manifest_at_its_own_urls_and_nowhere_else() {
        let hash = |n: u8| format!("{n:02x}").repeat(32);
        let text = [
            "BuildManifest.plist",
            "Image.dmg",
            "Image.dmg.trustcache",
            "Image.dmg.cryptex_info",
            "Image.dmg.root_hash",
        ]
        .iter()
        .zip(0_u8..)
        .map(|(name, n)| format!("{}  PersonalizedImages/Xcode_iOS_DDI_Cryptex/{name}\n", hash(n)))
        .collect::<Vec<_>>()
        .concat();
        let manifest = Manifest::parse(&text).unwrap();
        // A mirror on any host: `<repo>/<ref>/PersonalizedImages/<folder>/<name>`.
        let mirror = Mirror::new("https://mirror.example:8443/ddi/", "abc123", manifest.clone()).unwrap();
        assert_eq!(mirror.label(), "mirror.example:8443/ddi");
        let plan = Plan::mirror(&mirror, Variant::Cryptex).unwrap();
        assert_eq!(plan.build, None);
        assert_eq!(plan.total_bytes(), None);
        assert!(plan.subdir.starts_with("mirror-") && plan.subdir.len() == "mirror-".len() + 16, "{}", plan.subdir);
        let image = plan.files.iter().find(|f| f.name == "Image.dmg").unwrap();
        assert_eq!(
            image.url.as_text(),
            "https://mirror.example:8443/ddi/abc123/PersonalizedImages/Xcode_iOS_DDI_Cryptex/Image.dmg"
        );
        assert_eq!(image.sha256, hash(1));
        // A repository on github.com is read from its raw file service.
        let github = Mirror::new("https://github.com/me/DeveloperDiskImage.git", "abc123", manifest.clone()).unwrap();
        assert_eq!(github.label(), "github.com/me/DeveloperDiskImage");
        let plan = Plan::mirror(&github, Variant::Cryptex).unwrap();
        assert_eq!(
            plan.files[1].url.as_text(),
            "https://raw.githubusercontent.com/me/DeveloperDiskImage/abc123/PersonalizedImages/Xcode_iOS_DDI_Cryptex/Image.dmg"
        );
        // Another revision or another repository is another folder of the cache.
        let other = Mirror::new("https://mirror.example:8443/ddi", "def456", manifest.clone()).unwrap();
        assert_ne!(other.subdir, mirror.subdir);
        // The personalized folder is not in this manifest: that is said, and the mirror's files are not guessed.
        let why = Plan::mirror(&mirror, Variant::Personalized).unwrap_err();
        assert!(
            why.contains("no entry for PersonalizedImages/Xcode_iOS_DDI_Personalized/BuildManifest.plist"),
            "{why}"
        );
        // The folder-relative form of a path is taken too.
        let short = Manifest::parse(&format!("{}  Xcode_iOS_DDI_Personalized/BuildManifest.plist\n{}  Xcode_iOS_DDI_Personalized/Image.dmg\n{}  Xcode_iOS_DDI_Personalized/Image.dmg.trustcache\n", hash(7), hash(8), hash(9))).unwrap();
        let personalized =
            Plan::mirror(&Mirror::new("https://mirror.example/ddi", "abc", short).unwrap(), Variant::Personalized)
                .unwrap();
        assert_eq!(personalized.files[2].sha256, hash(9));
    }

    #[test]
    fn a_mirror_needs_an_https_repository_and_a_plain_revision() {
        let manifest = Manifest::default();
        for (repo, git_ref, want) in [
            ("http://mirror.example/ddi", "abc", "https"),
            ("ftp://mirror.example/ddi", "abc", "--ddi-repo"),
            ("https://mirror.example/ddi?token=secret", "abc", "query"),
            ("https://user:pw@mirror.example/ddi", "abc", "user name"),
            ("https://github.com/only-owner", "abc", "<owner>/<repo>"),
            ("https://github.com/a/b/c", "abc", "<owner>/<repo>"),
            ("https://mirror.example/ddi", "", "--ddi-ref"),
            ("https://mirror.example/ddi", "main/../x", "--ddi-ref"),
            ("https://mirror.example/ddi", "-abc", "--ddi-ref"),
            ("https://mirror.example/ddi", "a b", "--ddi-ref"),
        ] {
            let why = Mirror::new(repo, git_ref, manifest.clone()).unwrap_err();
            assert!(why.contains(want), "{repo} {git_ref}: {why}");
        }
        // What is in the message of a refused URL is not the URL: it can carry a secret.
        let why = Mirror::new("https://mirror.example/ddi?token=secret", "abc", manifest).unwrap_err();
        assert!(!why.contains("secret"), "{why}");
    }

    #[test]
    fn a_byte_count_is_said_in_megabytes_kilobytes_or_bytes() {
        assert_eq!(human(18_045_645), "18 MB");
        assert_eq!(human(17_239_040), "17 MB");
        assert_eq!(human(1_851), "2 KB");
        assert_eq!(human(428), "428 bytes");
        let said = FetchEvent::Downloading {
            source: "github.com/o/r".into(),
            build: Some("27B52".into()),
            bytes: Some(18_045_645),
        };
        assert_eq!(said.line(), "Downloading the developer disk image (Xcode 27B52) from github.com/o/r: 18 MB");
        let unknown = FetchEvent::Downloading { source: "mirror.example/ddi".into(), build: None, bytes: None };
        assert_eq!(unknown.line(), "Downloading the developer disk image from mirror.example/ddi");
        assert_eq!(FetchEvent::Progress { percent: 40 }.line(), "Downloading: 40%");
        assert!(FetchEvent::Verified.line().contains("SHA-256"));
    }

    #[test]
    fn the_meter_says_each_tenth_once() {
        let mut meter = Meter { total: Some(1000), last: None };
        let said: Vec<u8> =
            [0, 50, 99, 100, 150, 199, 200, 999, 1000, 1000, 5000].iter().filter_map(|done| meter.at(*done)).collect();
        assert_eq!(said, vec![0, 10, 20, 90, 100]);
        // No total, no percent; a total of zero is no total.
        assert_eq!(Meter { total: None, last: None }.at(5), None);
        assert_eq!(Meter { total: Some(0), last: None }.at(5), None);
    }

    /// For a person: `cargo test --lib --locked the_pinned_files_are_what_github_serves -- --ignored --nocapture`. Downloads the
    /// pinned files of both variants (some 35 MB) from GitHub through the program's own client, and checks them.
    #[tokio::test]
    #[ignore = "downloads some 35 MB from GitHub"]
    async fn the_pinned_files_are_what_github_serves() {
        for variant in [Variant::Cryptex, Variant::Personalized] {
            let plan = Plan::pinned(variant).unwrap();
            let cache = TempDir::new("fetch-real");
            let folder = plan.fetch(cache.path(), &Https, &mut |event| println!("{}", event.line())).await.unwrap();
            assert!(plan.cached(cache.path()).is_some());
            assert_eq!(folder.build().as_deref(), Some(pin::BUILD));
        }
    }
}
