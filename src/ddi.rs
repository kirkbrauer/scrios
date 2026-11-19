//! The developer disk image (DDI): where scrios finds one to install, and which of its files belong to a device.
//!
//! The display service is only offered to a device that has the developer disk image installed. iOS 17 and later take
//! the *personalized* image: a folder of the kind Xcode installs (`iOS_DDI`), with a `BuildManifest.plist`, the
//! image (`.dmg`) and its trust cache, which Apple's signing server (TSS) personalizes for the device. It goes onto
//! the device in one of two ways, which want different parts of that folder ([`Variant`]):
//!
//! * as a **cryptex** through `cryptexd`, over the `CoreDevice` tunnel (iOS 17.4 and later: it is what Xcode does, and
//!   the only way for the newest devices): the manifest's `Cryptex1,*` entries (image, trust cache, cryptex info and
//!   volume root hash);
//! * through the **image mounter** (iOS 17.0 to 17.3.1, and where `cryptexd` is not offered): the image and trust
//!   cache that the manifest names for the device's board and chip.
//!
//! scrios does not ship or host Apple's image. It looks, in this order, at:
//!
//! 1. the folder given (`--ddi-dir`),
//! 2. `$SCRIOS_DDI_DIR`,
//! 3. on macOS, the folders Xcode installs the image in,
//! 4. a per-user cache folder (see [`cache_dir`]): a folder copied there from a Mac with Xcode, an archive that was
//!    downloaded ([`Download`]: a URL and the SHA-256 it must match), or the files that were downloaded from the
//!    `DeveloperDiskImage` repository before ([`fetch`]: checked against their SHA-256 every time they are used);
//! 5. and, in [`DdiMode::Auto`] (the command line's default), a download of those files: from the commit that
//!    [`pin`] names (Apple's image as the pymobiledevice3 project's `DeveloperDiskImage` repository publishes it), or
//!    from a mirror the user names ([`Mirror`]) with a manifest of the SHA-256 values its files must have. Every
//!    file is checked against its size and hash before it is used; [`DdiMode::Local`] never downloads, and
//!    [`DdiMode::None`] installs nothing.
//!
//! In a folder, the build manifest is looked for in the folder, in `Restore/`, and under an `iOS_DDI/` folder in
//! either. For the image mounter the image and trust cache are the ones the manifest names for the device's board and
//! chip (Xcode's layout: `Restore/<name>.dmg` and `Restore/Firmware/<name>.dmg.trustcache`), else `Image4.dmg`,
//! `Image.dmg` or `DeveloperDiskImage.dmg` with a `.trustcache` next to it.

use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use idevice::cryptexd::Cryptex1Assets;
use sha2::{Digest, Sha256};
use tracing::{debug, info};

pub mod fetch;
pub mod http;
pub mod manifest;
pub mod pin;

use fetch::{Fetch, FetchEvent, Https, Mirror, Plan};
use manifest::Manifest;

/// The environment variable that names a DDI folder.
pub const ENV_DIR: &str = "SCRIOS_DDI_DIR";
/// The environment variable that names the URL of a DDI archive to download.
pub const ENV_URL: &str = "SCRIOS_DDI_URL";
/// The environment variable that holds the SHA-256 of the archive at [`ENV_URL`].
pub const ENV_SHA256: &str = "SCRIOS_DDI_SHA256";
/// The environment variable that names the mode: `auto`, `local` or `none` (see [`DdiMode`]).
pub const ENV_SOURCE: &str = "SCRIOS_DDI_SOURCE";
/// The environment variable that names a mirror of the `DeveloperDiskImage` repository.
pub const ENV_REPO: &str = "SCRIOS_DDI_REPO";
/// The environment variable that names the commit of the mirror to read.
pub const ENV_REF: &str = "SCRIOS_DDI_REF";
/// The environment variable that names the file of SHA-256 values of the mirror's files.
pub const ENV_MANIFEST: &str = "SCRIOS_DDI_SHA256_MANIFEST";

/// The largest archive that is unpacked (its entries' sizes added up): the image itself is some tens of MB.
const MAX_UNPACKED: u64 = 2 * 1024 * 1024 * 1024;
/// The manifest's file name.
const MANIFEST: &str = "BuildManifest.plist";
/// The folder of the cache that a downloaded archive is unpacked into.
const ARCHIVE_DIR: &str = "archive";
/// Names of the image in a folder that has no manifest path for it, most specific first.
const IMAGE_NAMES: [&str; 3] = ["Image4.dmg", "Image.dmg", "DeveloperDiskImage.dmg"];

/// Where a DDI folder came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// `--ddi-dir`.
    Option,
    /// `$SCRIOS_DDI_DIR`.
    Env,
    /// A folder Xcode installed (macOS).
    Xcode,
    /// scrios's own cache folder (a copy put there, or a download).
    Cache,
}

impl Origin {
    /// How a message names it.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Origin::Option => "--ddi-dir",
            Origin::Env => "$SCRIOS_DDI_DIR",
            Origin::Xcode => "Xcode",
            Origin::Cache => "scrios's cache",
        }
    }
}

/// Which form of the developer disk image a device takes (see the module's documentation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Variant {
    /// The cryptex that `cryptexd` installs: the manifest's `Cryptex1,*` entries (iOS 17.4 and later).
    Cryptex,
    /// The personalized image that the image mounter mounts (iOS 17.0 to 17.3.1, and the fallback).
    Personalized,
}

impl Variant {
    /// The name of the folder that holds this variant's files in the `DeveloperDiskImage` repository (and in the cache).
    #[must_use]
    pub fn folder(self) -> &'static str {
        match self {
            Variant::Cryptex => "Xcode_iOS_DDI_Cryptex",
            Variant::Personalized => "Xcode_iOS_DDI_Personalized",
        }
    }

    /// How a message names it.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Variant::Cryptex => "cryptex",
            Variant::Personalized => "personalized image",
        }
    }
}

/// The manifest entries that a cryptex install reads a file for.
const CRYPTEX_ENTRIES: [&str; 4] =
    ["Cryptex1,GenericDmg", "Cryptex1,GenericTrustCache", "Cryptex1,CryptexInfoPlist", "Cryptex1,GenericVolume"];

/// A path from a manifest or a download, as a relative path that stays inside the folder it is joined to: no `..`, no
/// root or drive, no backslash, no NUL. (What a manifest names is read from the disk and sent to a device.)
///
/// # Errors
///
/// A message that says what is wrong with the path.
pub fn safe_relative(path: &str) -> Result<PathBuf, String> {
    use std::path::Component;
    if path.is_empty() || path.contains(['\\', '\0']) {
        return Err(format!("{path:?} is not a relative path inside the folder"));
    }
    let mut clean = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir => {}
            _ => return Err(format!("{path:?} leaves the folder it is in")),
        }
    }
    if clean.as_os_str().is_empty() {
        return Err(format!("{path:?} is not a relative path inside the folder"));
    }
    Ok(clean)
}

/// An archive to download the DDI folder from, and the SHA-256 it must have. There is no default: scrios does not
/// know of a place to get Apple's image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Download {
    /// An `http://` or `https://` URL of a `.tar.gz` (or `.tar`) of the DDI folder.
    pub url: String,
    /// The SHA-256 of the archive, in lowercase hex.
    pub sha256: String,
}

impl Download {
    /// A download of `url`, which must hash to `sha256` (64 hex digits).
    ///
    /// # Errors
    ///
    /// A message if the URL is not http(s) or the hash is not 64 hex digits.
    pub fn new(url: &str, sha256: &str) -> Result<Download, String> {
        let url = url.trim();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(format!("the DDI download URL must start with http:// or https:// (got {:?})", host_of(url)));
        }
        let sha256 = sha256.trim().to_ascii_lowercase();
        if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("the DDI SHA-256 must be 64 hex digits".into());
        }
        Ok(Download { url: url.to_string(), sha256 })
    }
}

/// The host of a URL, for messages: nothing else of it (a URL can carry a token).
#[must_use]
pub fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    authority.rsplit('@').next().unwrap_or(authority).to_string()
}

/// A folder with a build manifest and the images it names.
#[derive(Debug, Clone)]
pub struct DdiFolder {
    /// The folder the manifest is in: the paths in the manifest are relative to it.
    root: PathBuf,
    manifest: PathBuf,
}

/// The files to mount for one device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DdiFiles {
    pub image: PathBuf,
    pub trust_cache: PathBuf,
}

/// The `Info.Path` of a manifest entry (`PersonalizedDMG`, `LoadableTrustCache`) of a build identity.
fn entry_path<'a>(identity: &'a plist::Dictionary, entry: &str) -> Option<&'a str> {
    identity
        .get("Manifest")?
        .as_dictionary()?
        .get(entry)?
        .as_dictionary()?
        .get("Info")?
        .as_dictionary()?
        .get("Path")?
        .as_string()
}

fn identities(manifest: &plist::Dictionary) -> Vec<&plist::Dictionary> {
    manifest
        .get("BuildIdentities")
        .and_then(plist::Value::as_array)
        .map(|all| all.iter().filter_map(plist::Value::as_dictionary).collect())
        .unwrap_or_default()
}

/// The `.trustcache` that goes with a conventional image name: next to it, or under `Firmware/`.
fn conventional_trust_cache(root: &Path, image_name: &str) -> Option<PathBuf> {
    let stem = image_name.strip_suffix(".dmg").unwrap_or(image_name);
    [
        format!("{image_name}.trustcache"),
        format!("Firmware/{image_name}.trustcache"),
        format!("{stem}.trustcache"),
        "Image4.trustcache".to_string(),
    ]
    .iter()
    .map(|name| root.join(name))
    .find(|path| path.is_file())
}

impl DdiFolder {
    /// The folder at `dir`, or the `iOS_DDI` folder in it: the one whose `BuildManifest.plist` (in the folder or in
    /// `Restore/`) is a build manifest with an image next to it.
    ///
    /// # Errors
    ///
    /// A message that says what is missing: the folder, the manifest, a manifest that is not one, or its images.
    pub fn open(dir: &Path) -> Result<DdiFolder, String> {
        if !dir.is_dir() {
            return Err(format!("{} is not a folder", dir.display()));
        }
        let manifest = ["", "Restore", "iOS_DDI", "iOS_DDI/Restore"]
            .iter()
            .map(|sub| dir.join(sub).join(MANIFEST))
            .find(|path| path.is_file())
            .ok_or_else(|| {
                format!(
                    "no {MANIFEST} in {} (looked in the folder, Restore/, iOS_DDI/ and iOS_DDI/Restore/): is it the iOS_DDI folder of Xcode?",
                    dir.display()
                )
            })?;
        let root = manifest.parent().map_or_else(|| dir.to_path_buf(), Path::to_path_buf);
        let folder = DdiFolder { root, manifest };
        let parsed = folder.read_manifest()?;
        if !identities(&parsed.1).iter().any(|id| folder.has_image(id)) {
            return Err(format!(
                "{} names no image that exists next to it: the folder is incomplete (copy all of iOS_DDI, not only the manifest)",
                folder.manifest.display()
            ));
        }
        Ok(folder)
    }

    /// The folder the manifest is in.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The build manifest.
    #[must_use]
    pub fn manifest(&self) -> &Path {
        &self.manifest
    }

    /// The manifest as it is on disk (what the mounter sends to Apple's signing server) and parsed.
    ///
    /// # Errors
    ///
    /// A message if the file cannot be read or is not a plist dictionary with `BuildIdentities`.
    pub fn read_manifest(&self) -> Result<(Vec<u8>, plist::Dictionary), String> {
        let bytes =
            std::fs::read(&self.manifest).map_err(|e| format!("cannot read {}: {e}", self.manifest.display()))?;
        let parsed: plist::Dictionary =
            plist::from_bytes(&bytes).map_err(|e| format!("{} is not a plist: {e}", self.manifest.display()))?;
        if identities(&parsed).is_empty() {
            return Err(format!("{} has no BuildIdentities: it is not a build manifest", self.manifest.display()));
        }
        Ok((bytes, parsed))
    }

    /// The build of the software the image was made for (`27B48`), if the manifest says.
    #[must_use]
    pub fn build(&self) -> Option<String> {
        let (_, manifest) = self.read_manifest().ok()?;
        manifest.get("ProductBuildVersion").and_then(plist::Value::as_string).map(str::to_string)
    }

    /// Whether the folder has what `variant` needs: for the personalized image, what [`DdiFolder::open`] has checked
    /// (an image next to the manifest; the device's own identity is looked for when it is mounted); for the cryptex,
    /// a cryptex build identity in the manifest and the four files it names (image, trust cache, cryptex info and
    /// volume root hash), each a path that stays inside the folder.
    ///
    /// # Errors
    ///
    /// A message that says what the folder lacks (an Xcode older than the cryptex install has no cryptex identity).
    pub fn check(&self, variant: Variant) -> Result<(), String> {
        match variant {
            Variant::Personalized => Ok(()),
            Variant::Cryptex => self.check_cryptex(),
        }
    }

    fn check_cryptex(&self) -> Result<(), String> {
        let (_, manifest) = self.read_manifest()?;
        let identity = idevice::tss::select_cryptex_build_identity(&manifest).map_err(|_| {
            format!(
                "the DDI in {} has no developer disk image cryptex (no `Cryptex1,*` build identity): it is from an Xcode older than the cryptex install; use the iOS_DDI folder of a newer one",
                self.root.display()
            )
        })?;
        for entry in CRYPTEX_ENTRIES {
            let named = entry_path(identity, entry).ok_or_else(|| {
                format!("the cryptex identity of {} names no file for {entry}", self.manifest.display())
            })?;
            let relative =
                safe_relative(named).map_err(|why| format!("{entry} in {}: {why}", self.manifest.display()))?;
            if !self.root.join(&relative).is_file() {
                return Err(format!(
                    "{} is missing in {}: the folder is incomplete (copy all of iOS_DDI)",
                    relative.display(),
                    self.root.display()
                ));
            }
        }
        Ok(())
    }

    /// The payloads of a cryptex install, read from the folder (idevice's [`Cryptex1Assets`]): the image, trust
    /// cache, cryptex info and volume root hash the cryptex identity names, and the identity itself.
    ///
    /// # Errors
    ///
    /// The reason the folder has no usable cryptex ([`DdiFolder::check`]), or why a file could not be read.
    pub async fn cryptex_assets(&self) -> Result<Cryptex1Assets, String> {
        self.check(Variant::Cryptex)?;
        Cryptex1Assets::load(&self.root)
            .await
            .map_err(|e| format!("cannot load the cryptex from {}: {e}", self.root.display()))
    }

    /// Whether the image that `identity` names (or a conventional one) is in the folder.
    fn has_image(&self, identity: &plist::Dictionary) -> bool {
        entry_path(identity, "PersonalizedDMG").is_some_and(|p| self.root.join(p).is_file())
            || IMAGE_NAMES.iter().any(|name| self.root.join(name).is_file())
    }

    /// The image and trust cache for the device with this board and chip ID (from the device's personalization
    /// identifiers): the ones its build identity names, else the conventional names.
    ///
    /// # Errors
    ///
    /// A message if the manifest has no build identity for the device (the image is from an Xcode that is older than
    /// the device's iOS), or the files it names are not in the folder.
    pub fn files_for(&self, board_id: u64, chip_id: u64) -> Result<DdiFiles, String> {
        let (_, manifest) = self.read_manifest()?;
        let identity = idevice::tss::select_build_identity(&manifest, board_id, chip_id, None).map_err(|_| {
            format!(
                "the DDI in {} has no build identity for this device (board {board_id:#x}, chip {chip_id:#x}): it is from an Xcode older than the device's iOS; copy the iOS_DDI folder of a newer one",
                self.root.display()
            )
        })?;
        let named_image = entry_path(identity, "PersonalizedDMG").map(|p| self.root.join(p)).filter(|p| p.is_file());
        let named_cache = entry_path(identity, "LoadableTrustCache").map(|p| self.root.join(p)).filter(|p| p.is_file());
        if let (Some(image), Some(trust_cache)) = (&named_image, &named_cache) {
            return Ok(DdiFiles { image: image.clone(), trust_cache: trust_cache.clone() });
        }
        for name in IMAGE_NAMES {
            let image = self.root.join(name);
            if image.is_file() {
                let trust_cache = named_cache.clone().or_else(|| conventional_trust_cache(&self.root, name));
                if let Some(trust_cache) = trust_cache {
                    return Ok(DdiFiles { image, trust_cache });
                }
            }
        }
        let wanted = |entry: &str| entry_path(identity, entry).unwrap_or("?").to_string();
        Err(format!(
            "the files the manifest names for this device are not in {}: {} and {} (copy all of iOS_DDI)",
            self.root.display(),
            wanted("PersonalizedDMG"),
            wanted("LoadableTrustCache")
        ))
    }
}

// ---- where to look ----------------------------------------------------------------------------

/// The kind of host, for the folders it keeps things in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Mac,
    Windows,
    /// Linux and the others, which keep a cache under `$XDG_CACHE_HOME` or `~/.cache`.
    Other,
}

impl Platform {
    /// The host scrios is running on.
    #[must_use]
    pub fn current() -> Platform {
        if cfg!(target_os = "macos") {
            Platform::Mac
        } else if cfg!(windows) {
            Platform::Windows
        } else {
            Platform::Other
        }
    }
}

/// The cache folder of scrios's DDI for `platform`, given the environment (`get` reads a variable):
/// `~/Library/Caches/scrios/ddi` on macOS, `%LOCALAPPDATA%\scrios\ddi` on Windows, `$XDG_CACHE_HOME/scrios/ddi` (or
/// `~/.cache/scrios/ddi`) elsewhere. `None` if the variables it needs are not set.
#[must_use]
pub fn cache_dir_for(platform: Platform, get: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let var = |name: &str| get(name).filter(|v| !v.trim().is_empty()).map(PathBuf::from);
    let base = match platform {
        Platform::Mac => var("HOME")?.join("Library").join("Caches"),
        Platform::Windows => var("LOCALAPPDATA")?,
        Platform::Other => match var("XDG_CACHE_HOME").filter(|p| p.is_absolute()) {
            Some(xdg) => xdg,
            None => var("HOME")?.join(".cache"),
        },
    };
    Some(base.join("scrios").join("ddi"))
}

/// The cache folder on this host (see [`cache_dir_for`]).
#[must_use]
pub fn cache_dir() -> Option<PathBuf> {
    cache_dir_for(Platform::current(), &|name| std::env::var(name).ok())
}

/// The folders Xcode may have installed the iOS image in. `selected` is what `xcode-select -p` printed (the
/// developer directory of the selected Xcode), if it printed anything. These paths are from what Xcode is known to
/// do, not checked against every Xcode: only the ones that exist are used.
#[must_use]
pub fn xcode_dirs_for(selected: Option<&str>) -> Vec<PathBuf> {
    let mut apps: Vec<PathBuf> = Vec::new();
    if let Some(app) = selected.and_then(|dir| dir.trim().strip_suffix("/Contents/Developer")) {
        apps.push(PathBuf::from(app));
    }
    let default_app = PathBuf::from("/Applications/Xcode.app");
    if !apps.contains(&default_app) {
        apps.push(default_app);
    }
    let mut dirs = vec![PathBuf::from("/Library/Developer/DeveloperDiskImages/iOS_DDI")];
    dirs.extend(apps.into_iter().map(|app| app.join("Contents/Resources/CoreDeviceDDIs/iOS_DDI")));
    dirs
}

/// The Xcode folders on this host: none unless this is macOS.
#[must_use]
pub fn xcode_dirs() -> Vec<PathBuf> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    let selected = std::process::Command::new("xcode-select")
        .arg("-p")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok());
    xcode_dirs_for(selected.as_deref())
}

/// Whether scrios may download an image it does not have, and whether it installs one at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DdiMode {
    /// Local sources first; if none has the image, download it. This is what the command line does by default.
    Auto,
    /// Local sources only: never the network. This is the default of the type, so that a program that builds
    /// [`DdiSources`] itself (or a test) never downloads unless it says so.
    #[default]
    Local,
    /// Nothing is installed or mounted: scrios only reports what it finds.
    None,
}

impl DdiMode {
    /// The mode that `text` (`auto`, `local`, `none`, in any case) names.
    ///
    /// # Errors
    ///
    /// A message that lists the three if it is none of them.
    pub fn parse(text: &str) -> Result<DdiMode, String> {
        match text.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(DdiMode::Auto),
            "local" => Ok(DdiMode::Local),
            "none" => Ok(DdiMode::None),
            other => Err(format!("the DDI source must be auto, local or none (got {other:?})")),
        }
    }
}

/// Where the files to download come from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Remote {
    /// The pinned commit of the `DeveloperDiskImage` repository ([`pin`]).
    #[default]
    Pinned,
    /// A mirror of it, with the manifest of SHA-256 values its files must have.
    Mirror(Mirror),
}

/// What an [`obtain`](DdiSources::obtain) says as it goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DdiEvent {
    /// An archive (`--ddi-download-url`) is being downloaded from this host (the rest of a URL can carry a secret).
    Archive { host: String },
    /// The files of the repository are being downloaded, or have been (see [`FetchEvent`]).
    Fetch(FetchEvent),
}

impl DdiEvent {
    /// The plain line to show for the event.
    #[must_use]
    pub fn line(&self) -> String {
        match self {
            DdiEvent::Archive { host } => {
                format!("Downloading the developer disk image from {host} (checked against the SHA-256 you gave)")
            }
            DdiEvent::Fetch(event) => event.line(),
        }
    }
}

/// What the command line and the environment say about where the image comes from: the options as they are given,
/// before the environment fills in what they leave out ([`DdiSources::from_options`]).
#[derive(Debug, Clone, Default)]
pub struct DdiOptions {
    /// `--ddi-dir`.
    pub dir: Option<PathBuf>,
    /// `--ddi-download-url`.
    pub download_url: Option<String>,
    /// `--ddi-sha256`.
    pub sha256: Option<String>,
    /// `--ddi-source`.
    pub source: Option<DdiMode>,
    /// `--ddi-repo`.
    pub repo: Option<String>,
    /// `--ddi-ref`.
    pub git_ref: Option<String>,
    /// `--ddi-sha256-manifest`.
    pub sha256_manifest: Option<PathBuf>,
}

/// Where to look for a DDI folder, in the order of the module's documentation.
///
/// `DdiSources::default()` has no folder anywhere and never downloads ([`DdiMode::Local`]); [`DdiSources::from_options`]
/// is what the command line uses.
#[derive(Debug, Clone, Default)]
pub struct DdiSources {
    /// `--ddi-dir`.
    pub dir: Option<PathBuf>,
    /// `$SCRIOS_DDI_DIR`.
    pub env_dir: Option<PathBuf>,
    /// The folders Xcode installs it in.
    pub xcode: Vec<PathBuf>,
    /// The cache folder.
    pub cache: Option<PathBuf>,
    /// The archive to download into the cache when nothing is found.
    pub download: Option<Download>,
    /// Whether to download, and whether to install.
    pub mode: DdiMode,
    /// Where the files are downloaded from in [`DdiMode::Auto`] when there is no archive.
    pub remote: Remote,
    /// What downloads the files: `None` is the program's own https client ([`Https`]).
    pub fetcher: Option<Arc<dyn Fetch>>,
}

/// A DDI folder that was found, and where.
#[derive(Debug, Clone)]
pub struct Found {
    pub folder: DdiFolder,
    pub origin: Origin,
}

impl Found {
    /// The folder, where it came from and the build it is for, as one phrase: `/x/Restore (Xcode, build 27B48)`.
    #[must_use]
    pub fn describe(&self) -> String {
        let build = self.folder.build().map_or_else(String::new, |b| format!(", build {b}"));
        format!("{} ({}{build})", self.folder.root().display(), self.origin.label())
    }
}

impl DdiSources {
    /// The sources on this host: `dir` and `url` and `sha256` are the options, the rest is read from the
    /// environment ([`ENV_DIR`], and [`ENV_URL`] with [`ENV_SHA256`] for what the options leave out) and the host.
    /// This is [`DdiSources::from_options`] with no other option: [`DdiMode::Auto`], the pinned image.
    ///
    /// # Errors
    ///
    /// A message if only one of the URL and the hash is given, or either is malformed.
    pub fn from_host(dir: Option<PathBuf>, url: Option<&str>, sha256: Option<&str>) -> Result<DdiSources, String> {
        DdiSources::from_options(&DdiOptions {
            dir,
            download_url: url.map(str::to_string),
            sha256: sha256.map(str::to_string),
            ..DdiOptions::default()
        })
    }

    /// The sources the command line asks for: `opts` are the options, and what they leave out is read from the
    /// environment ([`ENV_DIR`], [`ENV_URL`], [`ENV_SHA256`], [`ENV_SOURCE`], [`ENV_REPO`], [`ENV_REF`],
    /// [`ENV_MANIFEST`]) and the host. With nothing given the mode is [`DdiMode::Auto`] and the image is the pinned one.
    ///
    /// # Errors
    ///
    /// A message if the options do not go together (a mirror needs its revision and its manifest, and cannot be
    /// asked for with an archive; an archive needs its hash), or the manifest cannot be read or is malformed.
    pub fn from_options(opts: &DdiOptions) -> Result<DdiSources, String> {
        let mut sources = DdiSources::from_options_with(
            Platform::current(),
            opts,
            &|name| std::env::var(name).ok(),
            &read_manifest_file,
        )?;
        sources.xcode = xcode_dirs();
        Ok(sources)
    }

    /// [`DdiSources::from_options`] for `platform`, with the environment (`get`) and the reading of the manifest file
    /// (`read`) given.
    ///
    /// # Errors
    ///
    /// The same as [`DdiSources::from_options`].
    pub fn from_options_with(
        platform: Platform,
        opts: &DdiOptions,
        get: &dyn Fn(&str) -> Option<String>,
        read: &dyn Fn(&Path) -> Result<String, String>,
    ) -> Result<DdiSources, String> {
        let env = |name: &str| get(name).filter(|v| !v.trim().is_empty());
        let mut sources = DdiSources::for_platform(
            platform,
            opts.dir.clone(),
            env(ENV_DIR).map(PathBuf::from),
            opts.download_url.clone().or_else(|| env(ENV_URL)),
            opts.sha256.clone().or_else(|| env(ENV_SHA256)),
            get,
        )?;
        sources.mode = match (opts.source, env(ENV_SOURCE)) {
            (Some(mode), _) => mode,
            (None, Some(text)) => DdiMode::parse(&text).map_err(|why| format!("${ENV_SOURCE}: {why}"))?,
            (None, None) => DdiMode::Auto,
        };
        let repo = opts.repo.clone().or_else(|| env(ENV_REPO));
        let git_ref = opts.git_ref.clone().or_else(|| env(ENV_REF));
        let manifest = opts.sha256_manifest.clone().or_else(|| env(ENV_MANIFEST).map(PathBuf::from));
        sources.remote = match (repo, git_ref, manifest) {
            (None, None, None) => Remote::Pinned,
            (Some(repo), Some(git_ref), Some(manifest)) => {
                if sources.download.is_some() {
                    return Err("--ddi-repo and --ddi-download-url both say where to download from: give one".into());
                }
                let text = read(&manifest)?;
                let manifest = Manifest::parse(&text)
                    .map_err(|why| format!("--ddi-sha256-manifest {}: {why}", manifest.display()))?;
                Remote::Mirror(Mirror::new(&repo, &git_ref, manifest)?)
            }
            (Some(_), _, None) => {
                return Err("--ddi-repo needs --ddi-sha256-manifest too (a `sha256sum` list of the mirror's files): scrios never uses an unchecked download".into());
            }
            (Some(_), None, Some(_)) => {
                return Err("--ddi-repo needs --ddi-ref too (the commit of the mirror to read)".into());
            }
            (None, _, _) => {
                return Err("--ddi-ref and --ddi-sha256-manifest are for a mirror: they need --ddi-repo".into());
            }
        };
        Ok(sources)
    }

    /// The sources for `platform` with everything given (the Xcode folders are left to [`xcode_dirs`]).
    ///
    /// # Errors
    ///
    /// The same as [`DdiSources::from_host`].
    pub fn for_platform(
        platform: Platform,
        dir: Option<PathBuf>,
        env_dir: Option<PathBuf>,
        url: Option<String>,
        sha256: Option<String>,
        get: &dyn Fn(&str) -> Option<String>,
    ) -> Result<DdiSources, String> {
        let download = match (url, sha256) {
            (Some(url), Some(sha256)) => Some(Download::new(&url, &sha256)?),
            (None, None) => None,
            (Some(_), None) => {
                return Err("a DDI download needs its SHA-256 too (--ddi-sha256, or $SCRIOS_DDI_SHA256): scrios never uses an unchecked download".into());
            }
            (None, Some(_)) => {
                return Err("a DDI SHA-256 without a URL (--ddi-download-url, or $SCRIOS_DDI_URL)".into());
            }
        };
        Ok(DdiSources { dir, env_dir, cache: cache_dir_for(platform, get), download, ..DdiSources::default() })
    }

    /// The first DDI folder there is, of either variant (what a report says it would use when the device's way of
    /// installing is not known; see [`DdiSources::locate_for`]). A folder that was asked for (`--ddi-dir`,
    /// `$SCRIOS_DDI_DIR`) must be a DDI folder; Xcode's and the cache are tried only if they exist.
    ///
    /// # Errors
    ///
    /// The reason an explicit folder is not a DDI folder, or [`DdiSources::not_found`] if there is none to be found.
    pub fn locate(&self) -> Result<Found, String> {
        self.locate_by(None)
    }

    /// The first DDI folder that has what `variant` needs ([`DdiFolder::check`]). An explicit folder that lacks it is
    /// an error that says so; Xcode's and the cache are skipped if they lack it (and the message says that).
    ///
    /// # Errors
    ///
    /// As [`DdiSources::locate`].
    pub fn locate_for(&self, variant: Variant) -> Result<Found, String> {
        self.locate_by(Some(variant))
    }

    fn locate_by(&self, variant: Option<Variant>) -> Result<Found, String> {
        let open = |dir: &Path| -> Result<DdiFolder, String> {
            let folder = DdiFolder::open(dir)?;
            if let Some(variant) = variant {
                folder.check(variant)?;
            }
            Ok(folder)
        };
        if let Some(dir) = &self.dir {
            return open(dir)
                .map(|folder| Found { folder, origin: Origin::Option })
                .map_err(|why| format!("--ddi-dir: {why}"));
        }
        if let Some(dir) = &self.env_dir {
            return open(dir)
                .map(|folder| Found { folder, origin: Origin::Env })
                .map_err(|why| format!("${ENV_DIR}: {why}"));
        }
        let archive = self.cache.as_ref().map(|cache| cache.join(ARCHIVE_DIR));
        let tried = self
            .xcode
            .iter()
            .map(|dir| (dir, Origin::Xcode))
            .chain(self.cache.iter().map(|dir| (dir, Origin::Cache)))
            .chain(archive.iter().map(|dir| (dir, Origin::Cache)));
        let mut unusable = Vec::new();
        for (dir, origin) in tried {
            match open(dir) {
                Ok(folder) => return Ok(Found { folder, origin }),
                Err(why) => {
                    debug!("no usable DDI in {}: {why}", dir.display());
                    // A folder that is there and lacks what the variant needs is worth saying; one that is not there is not.
                    if dir.is_dir() && DdiFolder::open(dir).is_ok() {
                        unusable.push(why);
                    }
                }
            }
        }
        // The files that were downloaded before, which are checked again (their size and SHA-256) before they are used.
        let variants = variant.map_or_else(|| vec![Variant::Cryptex, Variant::Personalized], |v| vec![v]);
        if let Some(folder) = variants.into_iter().find_map(|variant| self.fetched(variant)) {
            return Ok(Found { folder, origin: Origin::Cache });
        }
        let mut why = self.not_found();
        if !unusable.is_empty() {
            let _ = write!(why, " (skipped: {})", unusable.join("; "));
        }
        Err(why)
    }

    /// What to tell a user who has no DDI folder: where scrios looked and how to provide one.
    #[must_use]
    pub fn not_found(&self) -> String {
        let cache = self.cache.as_ref().map_or_else(|| "its cache folder".to_string(), |p| p.display().to_string());
        // The manifest's place in the cache, with the separators of this host.
        let manifest = self.cache.as_ref().map_or_else(
            || format!("{cache}/Restore/{MANIFEST}"),
            |p| p.join("Restore").join(MANIFEST).display().to_string(),
        );
        let mut looked = vec!["--ddi-dir".to_string(), format!("${ENV_DIR}")];
        looked.extend(self.xcode.iter().map(|p| p.display().to_string()));
        looked.push(cache.clone());
        let way_out = match self.mode {
            DdiMode::Auto => " or let scrios download it (it does when nothing is found, unless the download fails)",
            DdiMode::Local => ", or let scrios download it (--ddi-source auto, the default: --ddi-source local is on)",
            DdiMode::None => ", or let scrios download it (--ddi-source auto, the default: --ddi-source none is on)",
        };
        format!(
            "no developer disk image (DDI) found (looked in {}): copy the iOS_DDI folder from a Mac with Xcode (/Library/Developer/DeveloperDiskImages/iOS_DDI) into {cache} so that {manifest} exists, or point --ddi-dir (or ${ENV_DIR}) at it{way_out}",
            looked.join(", ")
        )
    }

    /// What a report says about the image when none is found locally: what scrios would do about it.
    #[must_use]
    pub fn fallback_line(&self, variant: Variant) -> String {
        match self.mode {
            DdiMode::None => "none locally (--ddi-source none: scrios installs nothing)".to_string(),
            DdiMode::Local => "none locally (--ddi-source local: scrios does not download it)".to_string(),
            DdiMode::Auto => match (&self.download, self.plan_for(variant)) {
                (Some(download), _) => {
                    format!(
                        "none locally (scrios downloads the archive from {} when it installs the image)",
                        host_of(&download.url)
                    )
                }
                (None, Ok(plan)) => format!(
                    "none locally (scrios downloads {}from {} into {} when it installs the image)",
                    plan.build.as_ref().map_or_else(String::new, |build| format!("Xcode {build}'s, ")),
                    plan.source,
                    self.cache.as_ref().map_or_else(|| "its cache folder".to_string(), |c| c.display().to_string())
                ),
                (None, Err(why)) => format!("none locally ({why})"),
            },
        }
    }

    /// The plan to download `variant` from where [`DdiSources::remote`] says.
    ///
    /// # Errors
    ///
    /// A message if the mirror's manifest does not list the files of the variant.
    pub fn plan_for(&self, variant: Variant) -> Result<Plan, String> {
        match &self.remote {
            Remote::Pinned => Plan::pinned(variant),
            Remote::Mirror(mirror) => Plan::mirror(mirror, variant),
        }
    }

    /// The downloaded files of `variant` in the cache, if they are all there and are what they must be.
    fn fetched(&self, variant: Variant) -> Option<DdiFolder> {
        let cache = self.cache.as_ref()?;
        match self.plan_for(variant) {
            Ok(plan) => plan.cached(cache),
            Err(why) => {
                debug!("no plan to look for downloaded files by: {why}");
                None
            }
        }
    }

    /// Downloads the files of `variant` into the cache (the pinned image, or the mirror's), checks every one against
    /// its size and SHA-256, and returns the folder: what [`DdiSources::obtain`] does when nothing is found locally,
    /// without looking for anything locally first. A cache that is good already is used without any download.
    ///
    /// # Errors
    ///
    /// A message that says which file failed and why, or that there is no cache folder to keep the files in.
    pub async fn fetch_to_cache(
        &self,
        variant: Variant,
        events: &mut (dyn FnMut(DdiEvent) + Send),
    ) -> Result<Found, String> {
        let Some(cache) = &self.cache else {
            return Err("there is no cache folder to download into: HOME or LOCALAPPDATA is not set".into());
        };
        let plan = self.plan_for(variant)?;
        let https = Https;
        let fetcher: &dyn Fetch = self.fetcher.as_deref().unwrap_or(&https);
        let folder = Box::pin(plan.fetch(cache, fetcher, &mut |event| events(DdiEvent::Fetch(event)))).await?;
        Ok(Found { folder, origin: Origin::Cache })
    }

    /// [`DdiSources::locate`], and if there is none and a download is configured, the download into the cache
    /// (`downloading` is told the host it is downloaded from first: the rest of the URL can carry a secret).
    ///
    /// # Errors
    ///
    /// The reason there is no usable DDI folder: what [`DdiSources::locate`] says, or why the download failed.
    pub async fn find_or_download(&self, downloading: &mut (dyn FnMut(&str) + Send)) -> Result<Found, String> {
        Box::pin(self.obtain_by(None, &mut |event| {
            if let DdiEvent::Archive { host } = event {
                downloading(&host);
            }
        }))
        .await
    }

    /// [`DdiSources::find_or_download`] for a folder that has what `variant` needs ([`DdiSources::locate_for`]).
    ///
    /// # Errors
    ///
    /// As [`DdiSources::find_or_download`]; an archive that lacks what the variant needs is an error too.
    pub async fn find_or_download_for(
        &self,
        variant: Variant,
        downloading: &mut (dyn FnMut(&str) + Send),
    ) -> Result<Found, String> {
        Box::pin(self.obtain_by(Some(variant), &mut |event| {
            if let DdiEvent::Archive { host } = event {
                downloading(&host);
            }
        }))
        .await
    }

    /// The image in the form `variant`: the first local source that has it ([`DdiSources::locate_for`]) and, if there
    /// is none and the mode is [`DdiMode::Auto`], a download into the cache: of the archive, if one was configured,
    /// else of the pinned image (or the mirror's), every file checked against its size and SHA-256. `events` says what
    /// is downloaded and how far it is. A folder that was asked for by name (`--ddi-dir`, `$SCRIOS_DDI_DIR`) that does
    /// not work is an error, never replaced by a download.
    ///
    /// # Errors
    ///
    /// The reason there is no usable folder: what [`DdiSources::locate_for`] says, that downloading is off, or why
    /// the download failed.
    pub async fn obtain(&self, variant: Variant, events: &mut (dyn FnMut(DdiEvent) + Send)) -> Result<Found, String> {
        Box::pin(self.obtain_by(Some(variant), events)).await
    }

    async fn obtain_by(
        &self,
        variant: Option<Variant>,
        events: &mut (dyn FnMut(DdiEvent) + Send),
    ) -> Result<Found, String> {
        let first = self.locate_by(variant);
        let Err(why) = &first else { return first };
        if self.dir.is_some() || self.env_dir.is_some() {
            return first; // an explicit folder that does not work is not replaced by a download
        }
        match self.mode {
            DdiMode::Auto => {}
            DdiMode::Local => return Err(format!("{why} (downloading is off: --ddi-source local)")),
            DdiMode::None => return Err(format!("{why} (--ddi-source none: scrios installs nothing)")),
        }
        let Some(cache) = &self.cache else {
            return Err(format!(
                "{why} (and there is no cache folder to download into: HOME or LOCALAPPDATA is not set)"
            ));
        };
        if let Some(download) = &self.download {
            events(DdiEvent::Archive { host: host_of(&download.url) });
            let folder = Box::pin(fetch(download, &cache.join(ARCHIVE_DIR))).await?;
            if let Some(variant) = variant {
                folder.check(variant).map_err(|why| format!("the downloaded archive is no use: {why}"))?;
            }
            return Ok(Found { folder, origin: Origin::Cache });
        }
        Box::pin(self.fetch_to_cache(variant.unwrap_or(Variant::Cryptex), events)).await
    }
}

/// Reads the manifest of a mirror's SHA-256 values: a text file of at most [`manifest::MAX_BYTES`].
fn read_manifest_file(path: &Path) -> Result<String, String> {
    let meta =
        std::fs::metadata(path).map_err(|e| format!("cannot read the SHA-256 manifest {}: {e}", path.display()))?;
    if meta.len() > manifest::MAX_BYTES as u64 {
        return Err(format!("the SHA-256 manifest {} is larger than {} bytes", path.display(), manifest::MAX_BYTES));
    }
    std::fs::read_to_string(path).map_err(|e| format!("cannot read the SHA-256 manifest {}: {e}", path.display()))
}

// ---- the download -----------------------------------------------------------------------------

/// The lowercase hex of the SHA-256 of `bytes`.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    Sha256::digest(bytes).iter().fold(String::with_capacity(64), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    })
}

/// Downloads `download`, checks its hash, unpacks it and puts it in `dest` (replacing what is there), and opens it.
async fn fetch(download: &Download, dest: &Path) -> Result<DdiFolder, String> {
    let host = host_of(&download.url);
    let response = Box::pin(idevice::http::get(download.url.clone()))
        .await
        .map_err(|e| format!("cannot download the DDI from {host}: {e:?}"))?;
    if response.status != 200 {
        return Err(format!("cannot download the DDI from {host}: the server answered {}", response.status));
    }
    info!("downloaded {} bytes from {host}", response.body.len());
    install_archive(&response.body, &download.sha256, dest)
}

/// Checks that `archive` has the hash `sha256`, unpacks it next to `dest` and moves it into place.
fn install_archive(archive: &[u8], sha256: &str, dest: &Path) -> Result<DdiFolder, String> {
    let got = sha256_hex(archive);
    if got != sha256 {
        return Err(format!("the downloaded DDI has the SHA-256 {got}, not the {sha256} that was asked for: not used"));
    }
    let parent = dest.parent().ok_or_else(|| format!("{} has no parent folder", dest.display()))?;
    std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    let partial = parent.join(format!("ddi.partial-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&partial);
    std::fs::create_dir_all(&partial).map_err(|e| format!("cannot create {}: {e}", partial.display()))?;
    let result = unpack(archive, &partial).and_then(|()| DdiFolder::open(&partial).map(|_| ())).and_then(|()| {
        if dest.exists() {
            std::fs::remove_dir_all(dest).map_err(|e| format!("cannot replace {}: {e}", dest.display()))?;
        }
        std::fs::rename(&partial, dest).map_err(|e| format!("cannot move the DDI into {}: {e}", dest.display()))
    });
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&partial);
    }
    result?;
    DdiFolder::open(dest)
}

/// Unpacks a `.tar.gz` (or a plain `.tar`) into `dest`. Only files and folders are made; an entry that would leave
/// `dest` (a `..` or an absolute path) is an error, and links and devices are left out.
fn unpack(archive: &[u8], dest: &Path) -> Result<(), String> {
    let reader: Box<dyn Read + '_> = if archive.starts_with(&[0x1f, 0x8b]) {
        Box::new(flate2::read::GzDecoder::new(archive))
    } else {
        Box::new(archive)
    };
    let mut tar = tar::Archive::new(reader);
    let mut total: u64 = 0;
    for entry in tar.entries().map_err(|e| format!("the DDI archive is not a tar file: {e}"))? {
        let mut entry = entry.map_err(|e| format!("the DDI archive is damaged: {e}"))?;
        let kind = entry.header().entry_type();
        if !(kind.is_file() || kind.is_dir()) {
            debug!("DDI archive: leaving out an entry of type {kind:?}");
            continue;
        }
        total = total.saturating_add(entry.size());
        if total > MAX_UNPACKED {
            return Err("the DDI archive unpacks to more than 2 GiB: not used".into());
        }
        let name = entry.path().map(|p| p.display().to_string()).unwrap_or_default();
        let inside = entry.unpack_in(dest).map_err(|e| format!("cannot unpack {name} from the DDI archive: {e}"))?;
        if !inside {
            return Err(format!("the DDI archive has an entry that leaves its folder ({name}): not used"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;
    use crate::testdir::TempDir;

    /// A build manifest with two identities (an iPhone and an iPad, say), naming Xcode's layout.
    fn manifest(image: &str, cache: &str) -> Vec<u8> {
        let identity = |board: &str, chip: &str| {
            let info = |path: &str| {
                let mut d = plist::Dictionary::new();
                d.insert("Path".into(), path.into());
                let mut e = plist::Dictionary::new();
                e.insert("Info".into(), plist::Value::Dictionary(d));
                plist::Value::Dictionary(e)
            };
            let mut m = plist::Dictionary::new();
            m.insert("PersonalizedDMG".into(), info(image));
            m.insert("LoadableTrustCache".into(), info(cache));
            let mut id = plist::Dictionary::new();
            id.insert("ApBoardID".into(), board.into());
            id.insert("ApChipID".into(), chip.into());
            id.insert("Manifest".into(), plist::Value::Dictionary(m));
            plist::Value::Dictionary(id)
        };
        let mut top = plist::Dictionary::new();
        top.insert("ProductBuildVersion".into(), "27B48".into());
        top.insert(
            "BuildIdentities".into(),
            plist::Value::Array(vec![identity("0x0A", "0x8110"), identity("0x02", "0x8015")]),
        );
        let mut out = Vec::new();
        plist::to_writer_xml(&mut out, &plist::Value::Dictionary(top)).unwrap();
        out
    }

    /// A folder in Xcode's layout: `Restore/BuildManifest.plist`, `Restore/<name>.dmg`, `Restore/Firmware/..`.
    fn xcode_layout(root: &TempDir, prefix: &str) {
        root.write(
            &format!("{prefix}Restore/BuildManifest.plist"),
            &manifest("022-1.dmg", "Firmware/022-1.dmg.trustcache"),
        );
        root.write(&format!("{prefix}Restore/022-1.dmg"), b"image");
        root.write(&format!("{prefix}Restore/Firmware/022-1.dmg.trustcache"), b"cache");
    }

    #[test]
    fn xcodes_layout_is_read_through_the_manifest() {
        let dir = TempDir::new("ddi-xcode");
        xcode_layout(&dir, "");
        let folder = DdiFolder::open(dir.path()).unwrap();
        assert_eq!(folder.root(), dir.path().join("Restore"));
        assert_eq!(folder.build().as_deref(), Some("27B48"));
        let files = folder.files_for(0x0A, 0x8110).unwrap();
        assert_eq!(files.image, dir.path().join("Restore/022-1.dmg"));
        assert_eq!(files.trust_cache, dir.path().join("Restore/Firmware/022-1.dmg.trustcache"));
        assert_eq!(std::fs::read(files.image).unwrap(), b"image");
        // Another board is another identity of the same manifest; one that is not in it is explained.
        assert!(folder.files_for(0x02, 0x8015).is_ok());
        let why = folder.files_for(0x0B, 0x8130).unwrap_err();
        assert!(why.contains("no build identity") && why.contains("0xb") && why.contains("0x8130"), "{why}");
        // The folder above, and the iOS_DDI folder that was copied in whole, are found too.
        let copied = TempDir::new("ddi-copied");
        xcode_layout(&copied, "iOS_DDI/");
        assert_eq!(DdiFolder::open(copied.path()).unwrap().root(), copied.path().join("iOS_DDI/Restore"));
        assert!(DdiFolder::open(&dir.path().join("Restore")).is_ok());
    }

    #[test]
    fn a_flat_folder_with_the_conventional_names_works() {
        let dir = TempDir::new("ddi-flat");
        // The manifest names files that are not there; the conventional ones are.
        dir.write("BuildManifest.plist", &manifest("x.dmg", "Firmware/x.dmg.trustcache"));
        dir.write("Image4.dmg", b"image");
        dir.write("Image4.dmg.trustcache", b"cache");
        let folder = DdiFolder::open(dir.path()).unwrap();
        let files = folder.files_for(0x0A, 0x8110).unwrap();
        assert_eq!(files.image, dir.path().join("Image4.dmg"));
        assert_eq!(files.trust_cache, dir.path().join("Image4.dmg.trustcache"));
    }

    #[test]
    fn a_folder_that_is_not_a_ddi_says_what_is_missing() {
        let dir = TempDir::new("ddi-bad");
        assert!(DdiFolder::open(&dir.path().join("nope")).unwrap_err().contains("not a folder"));
        assert!(DdiFolder::open(dir.path()).unwrap_err().contains("no BuildManifest.plist"));
        dir.write("BuildManifest.plist", b"not a plist");
        assert!(DdiFolder::open(dir.path()).unwrap_err().contains("not a plist"));
        dir.write("BuildManifest.plist", &manifest("a.dmg", "a.trustcache"));
        assert!(DdiFolder::open(dir.path()).unwrap_err().contains("incomplete"));
        // The image is there, the trust cache is not.
        dir.write("a.dmg", b"image");
        let folder = DdiFolder::open(dir.path()).unwrap();
        assert!(folder.files_for(0x0A, 0x8110).unwrap_err().contains("a.trustcache"));
    }

    #[test]
    fn the_cache_folder_is_where_each_platform_keeps_caches() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| pairs.iter().find(|(k, _)| *k == name).map(|(_, v)| (*v).to_string())
        };
        assert_eq!(
            cache_dir_for(Platform::Mac, &env(&[("HOME", "/Users/me")])),
            Some(PathBuf::from("/Users/me/Library/Caches/scrios/ddi"))
        );
        assert_eq!(
            cache_dir_for(Platform::Other, &env(&[("HOME", "/home/me")])),
            Some(PathBuf::from("/home/me/.cache/scrios/ddi"))
        );
        // An absolute path is one with a drive on Windows, so the test's own absolute path is the host's.
        let absolute = if cfg!(windows) { "C:\\var\\cache\\me" } else { "/var/cache/me" };
        assert_eq!(
            cache_dir_for(Platform::Other, &|name| match name {
                "HOME" => Some("/home/me".to_string()),
                "XDG_CACHE_HOME" => Some(absolute.to_string()),
                _ => None,
            }),
            Some(PathBuf::from(absolute).join("scrios").join("ddi"))
        );
        // A relative XDG_CACHE_HOME is ignored, as the spec says.
        assert_eq!(
            cache_dir_for(Platform::Other, &env(&[("HOME", "/home/me"), ("XDG_CACHE_HOME", "cache")])),
            Some(PathBuf::from("/home/me/.cache/scrios/ddi"))
        );
        assert_eq!(
            cache_dir_for(Platform::Windows, &env(&[("LOCALAPPDATA", "C:\\Users\\me\\AppData\\Local")])),
            Some(PathBuf::from("C:\\Users\\me\\AppData\\Local").join("scrios").join("ddi"))
        );
        assert_eq!(cache_dir_for(Platform::Mac, &env(&[])), None);
        assert_eq!(cache_dir_for(Platform::Windows, &env(&[("HOME", "/x")])), None);
    }

    #[test]
    fn xcodes_folders_follow_the_selected_xcode() {
        let default = xcode_dirs_for(None);
        assert_eq!(default[0], PathBuf::from("/Library/Developer/DeveloperDiskImages/iOS_DDI"));
        assert_eq!(default[1], PathBuf::from("/Applications/Xcode.app/Contents/Resources/CoreDeviceDDIs/iOS_DDI"));
        assert_eq!(default.len(), 2);
        let chosen = xcode_dirs_for(Some("/Applications/Xcode-beta.app/Contents/Developer\n"));
        assert_eq!(chosen[1], PathBuf::from("/Applications/Xcode-beta.app/Contents/Resources/CoreDeviceDDIs/iOS_DDI"));
        assert_eq!(chosen.len(), 3);
        // Command line tools alone are not an Xcode.
        assert_eq!(xcode_dirs_for(Some("/Library/Developer/CommandLineTools")).len(), 2);
    }

    #[test]
    fn the_sources_are_tried_in_order() {
        let (flag, env, xcode, cache) =
            (TempDir::new("ddi-flag"), TempDir::new("ddi-env"), TempDir::new("ddi-x"), TempDir::new("ddi-cache"));
        for dir in [&flag, &env, &xcode, &cache] {
            xcode_layout(dir, "");
        }
        let sources = DdiSources {
            dir: Some(flag.path().into()),
            env_dir: Some(env.path().into()),
            xcode: vec![PathBuf::from("/nonexistent/xcode"), xcode.path().into()],
            cache: Some(cache.path().into()),
            ..DdiSources::default()
        };
        assert_eq!(sources.locate().unwrap().origin, Origin::Option);
        let sources = DdiSources { dir: None, ..sources };
        assert_eq!(sources.locate().unwrap().origin, Origin::Env);
        let sources = DdiSources { env_dir: None, ..sources };
        let found = sources.locate().unwrap();
        assert_eq!((found.origin, found.folder.root().to_path_buf()), (Origin::Xcode, xcode.path().join("Restore")));
        let sources = DdiSources { xcode: vec![], ..sources };
        assert_eq!(sources.locate().unwrap().origin, Origin::Cache);
        // A folder that was asked for and is no DDI is an error, not a fall back.
        let sources = DdiSources { dir: Some("/nonexistent/ddi".into()), ..sources };
        assert!(sources.locate().unwrap_err().starts_with("--ddi-dir: "));
        let sources = DdiSources { dir: None, env_dir: Some("/nonexistent/ddi".into()), ..sources };
        assert!(sources.locate().unwrap_err().starts_with("$SCRIOS_DDI_DIR: "));
    }

    #[test]
    fn with_nothing_found_the_instruction_says_where_to_put_one() {
        let sources = DdiSources {
            xcode: vec!["/nonexistent/xcode".into()],
            cache: Some("/nonexistent/cache/scrios/ddi".into()),
            ..DdiSources::default()
        };
        let why = sources.locate().unwrap_err();
        // The manifest's path is written with this host's separators.
        let manifest = Path::new("/nonexistent/cache/scrios/ddi").join("Restore").join("BuildManifest.plist");
        for want in [
            "no developer disk image",
            "--ddi-dir",
            "$SCRIOS_DDI_DIR",
            "/nonexistent/xcode",
            &manifest.display().to_string(),
            "copy the iOS_DDI folder from a Mac with Xcode",
        ] {
            assert!(why.contains(want), "{want} missing from {why}");
        }
    }

    #[test]
    fn a_download_needs_a_url_and_a_hash_that_look_right() {
        let hash = "AB".repeat(32);
        let download = Download::new(" https://example.com/ddi.tar.gz?token=secret ", &format!(" {hash} ")).unwrap();
        assert_eq!(download.sha256, "ab".repeat(32));
        assert!(Download::new("ftp://example.com/x", &hash).is_err());
        assert!(Download::new("https://example.com/x", "abc").is_err());
        assert!(Download::new("https://example.com/x", &"zz".repeat(32)).is_err());
        // The host is all a message shows of a URL.
        assert_eq!(host_of("https://user:pw@example.com:8443/a/b?token=secret"), "example.com:8443");
        let given = |url: Option<&str>, sha: Option<&str>| {
            DdiSources::for_platform(
                Platform::Other,
                None,
                None,
                url.map(str::to_string),
                sha.map(str::to_string),
                &|name| (name == "HOME").then(|| "/home/me".to_string()),
            )
        };
        assert!(given(None, None).unwrap().download.is_none());
        assert!(given(Some("https://example.com/x"), Some(&hash)).unwrap().download.is_some());
        assert!(given(Some("https://example.com/x"), None).unwrap_err().contains("SHA-256"));
        assert!(given(None, Some(&hash)).unwrap_err().contains("URL"));
        assert_eq!(given(None, None).unwrap().cache, Some(PathBuf::from("/home/me/.cache/scrios/ddi")));
    }

    /// The archive of an Xcode-layout folder, gzipped.
    fn archive_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        for (name, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            tar.append_data(&mut header, name, *data).unwrap();
        }
        let raw = tar.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&raw).unwrap();
        gz.finish().unwrap()
    }

    fn ddi_archive() -> Vec<u8> {
        archive_of(&[
            ("Restore/BuildManifest.plist", &manifest("022-1.dmg", "Firmware/022-1.dmg.trustcache")),
            ("Restore/022-1.dmg", b"image"),
            ("Restore/Firmware/022-1.dmg.trustcache", b"cache"),
        ])
    }

    #[test]
    fn an_archive_with_the_right_hash_is_unpacked_into_the_cache_replacing_what_was_there() {
        let cache = TempDir::new("ddi-dl");
        let dest = cache.path().join("scrios").join("ddi");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("old"), b"stale").unwrap();
        let archive = ddi_archive();
        let folder = install_archive(&archive, &sha256_hex(&archive), &dest).unwrap();
        assert_eq!(folder.root(), dest.join("Restore"));
        assert!(!dest.join("old").exists());
        assert_eq!(folder.files_for(0x0A, 0x8110).unwrap().image, dest.join("Restore/022-1.dmg"));
        // Nothing is left beside it.
        let siblings: Vec<_> = std::fs::read_dir(dest.parent().unwrap()).unwrap().collect();
        assert_eq!(siblings.len(), 1);
    }

    #[test]
    fn an_archive_with_the_wrong_hash_or_the_wrong_content_is_refused_and_leaves_nothing() {
        let cache = TempDir::new("ddi-dl-bad");
        let dest = cache.path().join("ddi");
        let archive = ddi_archive();
        let why = install_archive(&archive, &"0".repeat(64), &dest).unwrap_err();
        assert!(why.contains("SHA-256") && why.contains(&sha256_hex(&archive)), "{why}");
        assert!(!dest.exists());
        // A tar that is not a DDI folder.
        let other = archive_of(&[("readme.txt", b"hello")]);
        let why = install_archive(&other, &sha256_hex(&other), &dest).unwrap_err();
        assert!(why.contains("no BuildManifest.plist"), "{why}");
        assert!(!dest.exists());
        let leftovers: Vec<_> = std::fs::read_dir(cache.path()).unwrap().collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        // Bytes that are no archive at all.
        assert!(install_archive(b"hello", &sha256_hex(b"hello"), &dest).is_err());
    }

    #[test]
    fn an_archive_made_by_tar_in_the_folder_has_dot_slash_names_and_unpacks() {
        // `tar czf ddi.tar.gz -C iOS_DDI .` names its entries `./Restore/...`, with a `./` entry of its own.
        let archive = {
            let mut tar = tar::Builder::new(Vec::new());
            let mut dir = tar::Header::new_gnu();
            dir.set_entry_type(tar::EntryType::Directory);
            dir.set_mode(0o755);
            dir.set_size(0);
            tar.append_data(&mut dir, "./", std::io::empty()).unwrap();
            for (name, data) in [
                ("./Restore/BuildManifest.plist", manifest("022-1.dmg", "Firmware/022-1.dmg.trustcache")),
                ("./Restore/022-1.dmg", b"image".to_vec()),
                ("./Restore/Firmware/022-1.dmg.trustcache", b"cache".to_vec()),
            ] {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                tar.append_data(&mut header, name, data.as_slice()).unwrap();
            }
            tar.into_inner().unwrap()
        };
        let dir = TempDir::new("ddi-dotslash");
        let dest = dir.path().join("ddi");
        let folder = install_archive(&archive, &sha256_hex(&archive), &dest).unwrap();
        assert!(folder.files_for(0x0A, 0x8110).is_ok());
    }

    #[test]
    fn an_entry_that_leaves_the_folder_is_refused() {
        // `tar::Builder` will not write a `..` path, so the header is written by hand.
        let mut header = tar::Header::new_gnu();
        let name = b"../escape.txt";
        header.as_old_mut().name[..name.len()].copy_from_slice(name);
        header.set_size(1);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        let mut raw = header.as_bytes().to_vec();
        raw.extend_from_slice(&[b'x'; 1]);
        raw.resize(raw.len() + 511, 0);
        raw.extend_from_slice(&[0; 1024]);
        let dir = TempDir::new("ddi-escape");
        let dest = dir.path().join("inside");
        std::fs::create_dir_all(&dest).unwrap();
        let why = unpack(&raw, &dest).unwrap_err();
        assert!(why.contains("leaves its folder"), "{why}");
        assert!(!dir.path().join("escape.txt").exists());
    }

    #[tokio::test]
    async fn the_download_goes_through_http_and_is_checked() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;
        let archive = ddi_archive();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = archive.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(2) {
                let mut stream = stream.unwrap();
                // Read the whole request head before answering: closing with request bytes unread would reset the
                // connection, and the client could see the reset before the body.
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).is_ok_and(|n| n == 1) {
                    request.push(byte[0]);
                }
                let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
                // Close our side and let the client close its own (no unread data, so no reset).
                let _ = stream.shutdown(std::net::Shutdown::Write);
                let _ = stream.read_to_end(&mut Vec::new());
            }
        });
        let url = format!("http://127.0.0.1:{port}/ddi.tar.gz");
        let cache = TempDir::new("ddi-http");
        let dest = cache.path().join("ddi");
        let sources = |sha256: String| DdiSources {
            cache: Some(dest.clone()),
            download: Some(Download { url: url.clone(), sha256 }),
            mode: DdiMode::Auto,
            ..DdiSources::default()
        };
        let mut said = Vec::new();
        let found = Box::pin(sources(sha256_hex(&archive)).find_or_download(&mut |line| said.push(line.to_string())))
            .await
            .unwrap();
        assert_eq!(found.origin, Origin::Cache);
        // The archive is unpacked under archive/, so that it never replaces the files that were downloaded by build.
        assert_eq!(found.folder.root(), dest.join("archive").join("Restore"));
        assert_eq!(said, vec![format!("127.0.0.1:{port}")], "only the host is said");
        // The wrong hash is refused, and the cache keeps nothing of it.
        let _ = std::fs::remove_dir_all(&dest);
        let why = Box::pin(sources("0".repeat(64)).find_or_download(&mut |_| {})).await.unwrap_err();
        assert!(why.contains("not used"), "{why}");
        assert!(!dest.join("archive").exists());
    }

    #[tokio::test]
    async fn without_a_download_or_with_an_explicit_folder_nothing_is_fetched() {
        let none = DdiSources { cache: Some("/nonexistent/cache/ddi".into()), ..DdiSources::default() };
        assert!(Box::pin(none.find_or_download(&mut |_| {})).await.unwrap_err().contains("no developer disk image"));
        let download = Some(Download::new("http://127.0.0.1:1/x", &"0".repeat(64)).unwrap());
        let explicit = DdiSources { dir: Some("/nonexistent/ddi".into()), download, ..none };
        let why = Box::pin(explicit.find_or_download(&mut |_| {})).await.unwrap_err();
        assert!(why.starts_with("--ddi-dir: "), "{why}");
    }

    // ---- the cryptex variant ----------------------------------------------------------------------

    use crate::testddi;

    #[test]
    fn a_path_from_a_manifest_stays_inside_its_folder() {
        assert_eq!(safe_relative("Image.dmg").unwrap(), PathBuf::from("Image.dmg"));
        assert_eq!(safe_relative("./Firmware/a.dmg.trustcache").unwrap(), PathBuf::from("Firmware/a.dmg.trustcache"));
        for bad in ["", ".", "../x", "a/../../x", "/etc/passwd", "a\\b", "a\0b", "./"] {
            assert!(safe_relative(bad).is_err(), "{bad:?} was accepted");
        }
        if cfg!(windows) {
            assert!(safe_relative("C:/x").is_err());
        }
    }

    #[test]
    fn a_cryptex_folder_is_checked_for_its_identity_and_its_four_files() {
        let dir = TempDir::new("ddi-cryptex");
        testddi::cryptex_folder(&dir);
        let folder = DdiFolder::open(dir.path()).unwrap();
        assert!(folder.check(Variant::Cryptex).is_ok());
        assert_eq!(folder.build().as_deref(), Some("27B52"));
        // A file the cryptex identity names that is not there.
        std::fs::remove_file(dir.path().join("Image.dmg.root_hash")).unwrap();
        let why = folder.check(Variant::Cryptex).unwrap_err();
        assert!(why.contains("Image.dmg.root_hash") && why.contains("incomplete"), "{why}");
        // A folder with a personalized identity only (an Xcode from before the cryptex install).
        let old = TempDir::new("ddi-cryptex-old");
        testddi::personalized_folder(&old);
        let folder = DdiFolder::open(old.path()).unwrap();
        assert!(folder.check(Variant::Personalized).is_ok());
        let why = folder.check(Variant::Cryptex).unwrap_err();
        assert!(why.contains("no developer disk image cryptex") && why.contains("older"), "{why}");
        // A manifest that names a file outside its folder is refused before anything is read.
        let evil = TempDir::new("ddi-cryptex-evil");
        evil.write("BuildManifest.plist", &testddi::manifest(None, Some(["../../etc/passwd", "t", "i", "v"])));
        evil.write("Image.dmg", b"x");
        let why = DdiFolder::open(evil.path()).unwrap().check(Variant::Cryptex).unwrap_err();
        assert!(why.contains("Cryptex1,GenericDmg") && why.contains("leaves the folder"), "{why}");
    }

    #[tokio::test]
    async fn what_scrios_gives_cryptexd_is_the_four_payloads_and_the_identity_of_the_folder() {
        use idevice::cryptexd::{CryptexInstallRequest, DDI_IMAGE_TYPE_INDEX, DDI_NONCE_PERSISTENCE, DDI_PERSISTENCE};
        use idevice::xpc::XPCObject;
        let dir = TempDir::new("ddi-assets");
        testddi::cryptex_folder(&dir);
        let assets = DdiFolder::open(dir.path()).unwrap().cryptex_assets().await.unwrap();
        // The payloads are the files the identity names, the identity is the cryptex's.
        assert_eq!(assets.image, testddi::CRYPTEX_IMAGE);
        assert_eq!(assets.trustcache, testddi::CRYPTEX_TRUSTCACHE);
        assert_eq!(assets.info, testddi::CRYPTEX_INFO);
        assert_eq!(assets.volumehash, testddi::CRYPTEX_VOLUME);
        assert_eq!(assets.nonce_domain().unwrap(), testddi::NONCE_DOMAIN);
        // The `Cryptex1,*` parameters of the install request: unsigned integers (a signed one is refused by the
        // device), the identity's own values, and `MountedCryptex` false, as Xcode sends them.
        let properties = assets.cryptex1_properties().unwrap();
        let mut keys: Vec<&str> = properties.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "Cryptex1,NonceDomain",
                "Cryptex1,PreauthVersion",
                "Cryptex1,SubType",
                "Cryptex1,UseProductClass",
                "Cryptex1,Version",
                "MountedCryptex",
            ]
        );
        assert!(
            matches!(properties.get("Cryptex1,NonceDomain"), Some(XPCObject::UInt64(n)) if *n == testddi::NONCE_DOMAIN)
        );
        assert!(matches!(properties.get("Cryptex1,SubType"), Some(XPCObject::UInt64(2))));
        assert!(matches!(properties.get("MountedCryptex"), Some(XPCObject::Bool(false))));
        // The request that `install_ddi` makes of them: Xcode's DDI parameters.
        let request = CryptexInstallRequest::new(
            assets.image.clone(),
            assets.trustcache.clone(),
            b"ticket".to_vec(),
            assets.info.clone(),
            assets.volumehash.clone(),
            properties,
        );
        assert_eq!(
            (request.image_type_index, request.persistence, request.nonce_persistence, request.auth),
            (DDI_IMAGE_TYPE_INDEX, DDI_PERSISTENCE, DDI_NONCE_PERSISTENCE, 0)
        );
        assert_eq!((DDI_IMAGE_TYPE_INDEX, DDI_PERSISTENCE, DDI_NONCE_PERSISTENCE), (10, 2, 1));
        // And the ticket request it signs: the identity has what Apple's signing server is asked about.
        let mut chip = plist::Dictionary::new();
        chip.insert("img4_chip_chip".into(), 0x8140_u64.into());
        chip.insert("img4_chip_ecid".into(), 0x1234_u64.into());
        chip.insert("img4_chip_cpro".into(), true.into());
        let mut ticket = idevice::tss::TSSRequest::new();
        ticket.add_cryptex1_tags(&assets.build_identity, &chip, &[1, 2, 3]).unwrap();
        let sent = format!("{ticket:?}");
        for want in ["@Cryptex1,Ticket", "Cryptex1,ChipID", "Cryptex1,Nonce", "Cryptex1,UDID"] {
            assert!(sent.contains(want), "{want} missing from the ticket request");
        }
        // A folder that has no cryptex gives no assets.
        let old = TempDir::new("ddi-assets-old");
        testddi::personalized_folder(&old);
        let why = DdiFolder::open(old.path()).unwrap().cryptex_assets().await.unwrap_err();
        assert!(why.contains("no developer disk image cryptex"), "{why}");
    }

    #[test]
    fn the_source_that_has_the_form_the_device_takes_is_the_one_found() {
        let (xcode, cache) = (TempDir::new("ddi-form-x"), TempDir::new("ddi-form-c"));
        testddi::personalized_folder(&xcode);
        testddi::cryptex_folder(&cache);
        let sources =
            DdiSources { xcode: vec![xcode.path().into()], cache: Some(cache.path().into()), ..DdiSources::default() };
        // Xcode's folder is first and will do for the image mounter; the cryptex is the cache's.
        assert_eq!(sources.locate_for(Variant::Personalized).unwrap().origin, Origin::Xcode);
        assert_eq!(sources.locate_for(Variant::Cryptex).unwrap().origin, Origin::Cache);
        assert_eq!(sources.locate().unwrap().origin, Origin::Xcode);
        // With only a folder that has no cryptex, the message says what was skipped and why.
        let sources = DdiSources { cache: None, ..sources };
        let why = sources.locate_for(Variant::Cryptex).unwrap_err();
        assert!(why.contains("no developer disk image") && why.contains("skipped") && why.contains("cryptex"), "{why}");
        // A folder that was asked for and has no cryptex is an error, not a fall back.
        let explicit = DdiSources { dir: Some(xcode.path().into()), cache: Some(cache.path().into()), ..sources };
        let why = explicit.locate_for(Variant::Cryptex).unwrap_err();
        assert!(why.starts_with("--ddi-dir: ") && why.contains("no developer disk image cryptex"), "{why}");
        assert_eq!(explicit.locate_for(Variant::Personalized).unwrap().origin, Origin::Option);
        // What a report says it found.
        let found = explicit.locate_for(Variant::Personalized).unwrap();
        assert_eq!(found.describe(), format!("{} (--ddi-dir, build 27B52)", xcode.path().display()));
    }

    #[test]
    fn the_variants_name_their_folders_in_the_repository() {
        assert_eq!(Variant::Cryptex.folder(), "Xcode_iOS_DDI_Cryptex");
        assert_eq!(Variant::Personalized.folder(), "Xcode_iOS_DDI_Personalized");
        assert_ne!(Variant::Cryptex.label(), Variant::Personalized.label());
    }

    // ---- where the image comes from: the mode, a mirror, the download ----------------------------

    use crate::testnet::FakeFetch;

    use crate::testddi::mirror_world;

    async fn obtained(sources: &DdiSources, variant: Variant) -> (Result<Found, String>, Vec<DdiEvent>) {
        let mut events = Vec::new();
        let found = sources.obtain(variant, &mut |event| events.push(event)).await;
        (found, events)
    }

    #[tokio::test]
    async fn with_nothing_local_the_image_is_downloaded_once_checked_and_then_found_in_the_cache() {
        let (sources, fake, _cache) = mirror_world();
        assert!(sources.locate_for(Variant::Cryptex).is_err());
        let (found, events) = obtained(&sources, Variant::Cryptex).await;
        let found = found.unwrap();
        assert_eq!(found.origin, Origin::Cache);
        assert!(found.folder.root().starts_with(sources.cache.as_ref().unwrap()));
        assert!(found.folder.root().ends_with("Xcode_iOS_DDI_Cryptex"));
        assert_eq!(fake.asked().len(), 5);
        assert!(
            fake.asked().iter().all(|url| url.starts_with("https://mirror.example/ddi/abc123/PersonalizedImages/"))
        );
        // What was said: from where, how far, and that it is verified.
        assert!(
            matches!(&events[0], DdiEvent::Fetch(FetchEvent::Downloading { source, .. }) if source == "mirror.example/ddi")
        );
        assert_eq!(events.last(), Some(&DdiEvent::Fetch(FetchEvent::Verified)));
        // The files are found from then on without the network, by locate (a report) and by obtain (an install).
        assert_eq!(sources.locate_for(Variant::Cryptex).unwrap().origin, Origin::Cache);
        let (again, events) = obtained(&sources, Variant::Cryptex).await;
        assert!(again.is_ok());
        assert!(events.is_empty(), "{events:?}");
        assert_eq!(fake.asked().len(), 5, "nothing was asked for the second time");
        // A file of the cache that is damaged is not found, and is downloaded again.
        std::fs::write(found.folder.root().join("Image.dmg"), b"damaged").unwrap();
        assert!(sources.locate_for(Variant::Cryptex).is_err());
        assert!(obtained(&sources, Variant::Cryptex).await.0.is_ok());
        assert_eq!(fake.asked().len(), 6);
    }

    #[tokio::test]
    async fn the_mode_decides_whether_anything_is_downloaded() {
        // local: never; none: never, and says that nothing is installed.
        let (auto, fake, _cache) = mirror_world();
        let local = DdiSources { mode: DdiMode::Local, ..auto.clone() };
        let why = obtained(&local, Variant::Cryptex).await.0.unwrap_err();
        assert!(
            why.contains("no developer disk image") && why.contains("downloading is off: --ddi-source local"),
            "{why}"
        );
        let none = DdiSources { mode: DdiMode::None, ..auto.clone() };
        let why = obtained(&none, Variant::Cryptex).await.0.unwrap_err();
        assert!(why.contains("--ddi-source none: scrios installs nothing"), "{why}");
        assert!(fake.asked().is_empty());
        // The local copies are used in every mode, and no mode downloads over one.
        let xcode = TempDir::new("ddi-mode-xcode");
        testddi::cryptex_folder(&xcode);
        for mode in [DdiMode::Auto, DdiMode::Local, DdiMode::None] {
            let sources = DdiSources { mode, xcode: vec![xcode.path().into()], ..auto.clone() };
            assert_eq!(obtained(&sources, Variant::Cryptex).await.0.unwrap().origin, Origin::Xcode, "{mode:?}");
        }
        assert!(fake.asked().is_empty());
        // A folder from an Xcode that has no cryptex does not stop the download of one.
        let old = TempDir::new("ddi-mode-old");
        testddi::personalized_folder(&old);
        let sources = DdiSources { xcode: vec![old.path().into()], ..auto.clone() };
        assert_eq!(obtained(&sources, Variant::Cryptex).await.0.unwrap().origin, Origin::Cache);
        assert_eq!(fake.asked().len(), 5);
        // A folder that was asked for by name and does not work is an error, whatever the mode: no download.
        let (auto, fake, _cache) = mirror_world();
        let explicit = DdiSources { dir: Some("/nonexistent/ddi".into()), ..auto };
        let why = obtained(&explicit, Variant::Cryptex).await.0.unwrap_err();
        assert!(why.starts_with("--ddi-dir: "), "{why}");
        assert!(fake.asked().is_empty());
    }

    #[tokio::test]
    async fn a_download_that_is_not_what_the_manifest_says_is_refused_and_nothing_is_kept() {
        let (sources, fake, cache) = mirror_world();
        let url = "https://mirror.example/ddi/abc123/PersonalizedImages/Xcode_iOS_DDI_Cryptex/Image.dmg";
        fake.answer(url, Ok(b"not the image".to_vec()));
        let (found, events) = obtained(&sources, Variant::Cryptex).await;
        let why = found.unwrap_err();
        assert!(why.contains("Image.dmg has the SHA-256") && why.contains("not used"), "{why}");
        assert!(!events.contains(&DdiEvent::Fetch(FetchEvent::Verified)));
        assert!(sources.locate_for(Variant::Cryptex).is_err());
        let mut kept = Vec::new();
        let mut stack = vec![cache.path().to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    kept.push(path.file_name().unwrap().to_string_lossy().to_string());
                }
            }
        }
        assert!(!kept.iter().any(|name| name == "Image.dmg" || name.contains("partial")), "{kept:?}");
        // The other form of the image is not in the manifest: said, and nothing is guessed or asked for.
        let asked = fake.asked().len();
        let why = obtained(&sources, Variant::Personalized).await.0.unwrap_err();
        assert!(why.contains("no entry for PersonalizedImages/Xcode_iOS_DDI_Personalized"), "{why}");
        assert_eq!(fake.asked().len(), asked);
    }

    #[tokio::test]
    async fn the_pinned_image_is_downloaded_from_the_pinned_commit_and_only_accepted_if_it_matches_the_pin() {
        let fake = Arc::new(FakeFetch::default());
        let cache = TempDir::new("ddi-pinned-cache");
        let sources = DdiSources {
            cache: Some(cache.path().join("ddi")),
            mode: DdiMode::Auto,
            fetcher: Some(fake.clone()),
            ..DdiSources::default()
        };
        assert_eq!(sources.remote, Remote::Pinned);
        // A server that answers with something else for everything (what a wrong host or a replaced file is).
        for file in pin::CRYPTEX_FILES {
            let url = format!(
                "https://{}/{}/{}/PersonalizedImages/Xcode_iOS_DDI_Cryptex/{}",
                pin::HOST,
                pin::REPO,
                pin::COMMIT,
                file.name
            );
            fake.answer(&url, Ok(b"something else".to_vec()));
        }
        let why = obtained(&sources, Variant::Cryptex).await.0.unwrap_err();
        assert!(why.contains("BuildManifest.plist is 14 bytes, not the 804097 that were expected"), "{why}");
        assert_eq!(fake.asked().len(), 1, "the first file that is wrong ends it");
        assert!(
            fake.asked()[0].starts_with(&format!(
                "https://raw.githubusercontent.com/doronz88/DeveloperDiskImage/{}/",
                pin::COMMIT
            ))
        );
        assert!(sources.locate_for(Variant::Cryptex).is_err());
        // The report says what scrios would do.
        let line = sources.fallback_line(Variant::Cryptex);
        assert!(line.contains("Xcode 27B52") && line.contains("github.com/doronz88/DeveloperDiskImage"), "{line}");
        // With no cache folder to keep files in there is no download.
        let nowhere = DdiSources { cache: None, ..sources };
        let why = obtained(&nowhere, Variant::Cryptex).await.0.unwrap_err();
        assert!(why.contains("no cache folder"), "{why}");
    }

    #[test]
    fn what_a_report_says_when_nothing_is_found_depends_on_the_mode() {
        let sources = DdiSources { cache: Some("/c/scrios/ddi".into()), ..DdiSources::default() };
        let auto = DdiSources { mode: DdiMode::Auto, ..sources.clone() };
        let line = auto.fallback_line(Variant::Cryptex);
        assert!(
            line.starts_with(
                "none locally (scrios downloads Xcode 27B52's, from github.com/doronz88/DeveloperDiskImage into "
            ),
            "{line}"
        );
        assert!(line.contains(&Path::new("/c/scrios/ddi").display().to_string()), "{line}");
        assert!(sources.fallback_line(Variant::Cryptex).contains("--ddi-source local"));
        assert!(
            DdiSources { mode: DdiMode::None, ..sources.clone() }
                .fallback_line(Variant::Cryptex)
                .contains("installs nothing")
        );
        let archive = DdiSources {
            download: Some(Download::new("https://user:pw@example.com/ddi.tar.gz?t=1", &"0".repeat(64)).unwrap()),
            ..auto
        };
        let line = archive.fallback_line(Variant::Cryptex);
        assert!(line.contains("archive from example.com") && !line.contains("pw") && !line.contains("t=1"), "{line}");
        // The instruction when nothing is found says how to get an image in each mode.
        assert!(sources.not_found().contains("--ddi-source auto"));
        assert!(archive.not_found().contains("download it"));
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn no_file(path: &Path) -> Result<String, String> {
        Err(format!("cannot read {}", path.display()))
    }

    #[test]
    fn the_options_become_sources_and_the_environment_fills_in_what_they_leave_out() {
        let build = |opts: &DdiOptions, env: &'static [(&str, &str)]| {
            DdiSources::from_options_with(
                Platform::Other,
                opts,
                &move |name| {
                    env.iter()
                        .find(|(k, _)| *k == name)
                        .map(|(_, v)| (*v).to_string())
                        .or_else(|| (name == "HOME").then(|| "/home/me".to_string()))
                },
                &no_file,
            )
        };
        // Nothing given: the command line's own mode, the pinned image, the cache.
        let default = build(&DdiOptions::default(), &[]).unwrap();
        assert_eq!((default.mode, default.remote.clone()), (DdiMode::Auto, Remote::Pinned));
        assert!(default.download.is_none() && default.fetcher.is_none());
        assert_eq!(default.cache, Some(PathBuf::from("/home/me/.cache/scrios/ddi")));
        // The mode: the option, else the environment, else auto; and a bad word says where it came from.
        let given = |mode| DdiOptions { source: Some(mode), ..DdiOptions::default() };
        assert_eq!(build(&given(DdiMode::None), &[]).unwrap().mode, DdiMode::None);
        assert_eq!(build(&DdiOptions::default(), &[("SCRIOS_DDI_SOURCE", "Local")]).unwrap().mode, DdiMode::Local);
        assert_eq!(build(&given(DdiMode::Auto), &[("SCRIOS_DDI_SOURCE", "none")]).unwrap().mode, DdiMode::Auto);
        let why = build(&DdiOptions::default(), &[("SCRIOS_DDI_SOURCE", "sometimes")]).unwrap_err();
        assert!(why.starts_with("$SCRIOS_DDI_SOURCE: ") && why.contains("auto, local or none"), "{why}");
        assert_eq!(DdiMode::default(), DdiMode::Local, "the type never downloads unless it is told to");
        assert!(DdiSources::default().fetcher.is_none() && DdiSources::default().remote == Remote::Pinned);
    }

    #[test]
    fn a_mirror_needs_its_revision_and_its_manifest_and_is_not_an_archive() {
        let hash = "ab".repeat(32);
        let manifest_text: String = [
            "BuildManifest.plist",
            "Image.dmg",
            "Image.dmg.trustcache",
            "Image.dmg.cryptex_info",
            "Image.dmg.root_hash",
        ]
        .iter()
        .map(|n| format!("{hash}  PersonalizedImages/Xcode_iOS_DDI_Cryptex/{n}\n"))
        .collect::<Vec<_>>()
        .concat();
        let read_ok = |path: &Path| {
            assert_eq!(path, Path::new("/m/ddi.sha256"));
            Ok(manifest_text.clone())
        };
        let build = |opts: DdiOptions, env: &'static [(&str, &str)]| {
            DdiSources::from_options_with(
                Platform::Other,
                &opts,
                &move |name| {
                    env.iter()
                        .find(|(k, _)| *k == name)
                        .map(|(_, v)| (*v).to_string())
                        .or_else(|| (name == "HOME").then(|| "/home/me".to_string()))
                },
                &read_ok,
            )
        };
        let all = DdiOptions {
            repo: Some("https://mirror.example/ddi".into()),
            git_ref: Some("abc123".into()),
            sha256_manifest: Some("/m/ddi.sha256".into()),
            ..DdiOptions::default()
        };
        let mirror = build(all.clone(), &[]).unwrap();
        assert!(matches!(&mirror.remote, Remote::Mirror(m) if m.label() == "mirror.example/ddi"));
        // The environment says the same things as the options.
        let env = build(
            DdiOptions::default(),
            &[
                ("SCRIOS_DDI_REPO", "https://mirror.example/ddi"),
                ("SCRIOS_DDI_REF", "abc123"),
                ("SCRIOS_DDI_SHA256_MANIFEST", "/m/ddi.sha256"),
            ],
        )
        .unwrap();
        assert_eq!(env.remote, mirror.remote);
        // A mirror without its manifest or its revision, and a manifest or a revision without a mirror, are refused.
        let without = |f: fn(&mut DdiOptions)| {
            let mut opts = all.clone();
            f(&mut opts);
            build(opts, &[]).unwrap_err()
        };
        assert!(without(|o| o.sha256_manifest = None).contains("--ddi-sha256-manifest"));
        assert!(without(|o| o.git_ref = None).contains("--ddi-ref"));
        assert!(without(|o| o.repo = None).contains("need --ddi-repo"));
        assert!(
            build(DdiOptions { git_ref: Some("abc".into()), ..DdiOptions::default() }, &[])
                .unwrap_err()
                .contains("need --ddi-repo")
        );
        assert!(
            build(DdiOptions { repo: Some("https://mirror.example/ddi".into()), ..DdiOptions::default() }, &[])
                .unwrap_err()
                .contains("never uses an unchecked download")
        );
        // A mirror and an archive are two answers to one question.
        let both = DdiOptions {
            download_url: Some("https://example.com/x.tgz".into()),
            sha256: Some(hash.clone()),
            ..all.clone()
        };
        assert!(build(both, &[]).unwrap_err().contains("give one"));
        // The mirror itself: https, a plain revision.
        assert!(without(|o| o.repo = Some("http://mirror.example/ddi".into())).contains("--ddi-repo"));
        assert!(without(|o| o.git_ref = Some("main/../x".into())).contains("--ddi-ref"));
        // A manifest that cannot be read or is not one.
        let unreadable = DdiSources::from_options_with(Platform::Other, &all, &no_env, &no_file).unwrap_err();
        assert!(unreadable.contains("cannot read /m/ddi.sha256"), "{unreadable}");
        let garbage = DdiSources::from_options_with(Platform::Other, &all, &no_env, &|_| Ok("not a manifest\n".into()))
            .unwrap_err();
        assert!(garbage.starts_with("--ddi-sha256-manifest /m/ddi.sha256: "), "{garbage}");
        // The file reader itself: a file that is too large is not read.
        let dir = TempDir::new("ddi-manifest-file");
        let big = dir.write("big", &vec![b'#'; manifest::MAX_BYTES + 1]);
        assert!(read_manifest_file(&big).unwrap_err().contains("larger than"));
        let ok = dir.write("ok", format!("{hash}  a\n").as_bytes());
        assert!(read_manifest_file(&ok).unwrap().contains("  a"));
        assert!(read_manifest_file(&dir.path().join("missing")).is_err());
    }

    #[tokio::test]
    async fn every_source_is_tried_in_its_place_down_to_the_download() {
        let (sources, fake, _cache) = mirror_world();
        let cache_root = sources.cache.clone().unwrap();
        let (flag, env, xcode, copied, archive) = (
            TempDir::new("ddi-o-flag"),
            TempDir::new("ddi-o-env"),
            TempDir::new("ddi-o-x"),
            TempDir::new("ddi-o-copy"),
            TempDir::new("ddi-o-archive"),
        );
        for dir in [&flag, &env, &xcode, &copied, &archive] {
            testddi::cryptex_folder(dir);
        }
        let find = |sources: &DdiSources| {
            sources.locate_for(Variant::Cryptex).map(|found| (found.origin, found.folder.root().to_path_buf()))
        };
        // Downloaded files are the last of the local sources.
        obtained(&sources, Variant::Cryptex).await.0.unwrap();
        let fetched = find(&sources).unwrap();
        assert_eq!(fetched.0, Origin::Cache);
        assert!(fetched.1.starts_with(&cache_root) && fetched.1.ends_with("Xcode_iOS_DDI_Cryptex"));
        // ... after an archive that was downloaded (cache/archive), which is after a folder copied into the cache.
        std::fs::create_dir_all(cache_root.join("archive")).unwrap();
        for entry in std::fs::read_dir(archive.path()).unwrap() {
            let path = entry.unwrap().path();
            std::fs::copy(&path, cache_root.join("archive").join(path.file_name().unwrap())).unwrap();
        }
        assert_eq!(find(&sources).unwrap().1, cache_root.join("archive"));
        for entry in std::fs::read_dir(copied.path()).unwrap() {
            let path = entry.unwrap().path();
            std::fs::copy(&path, cache_root.join(path.file_name().unwrap())).unwrap();
        }
        assert_eq!(find(&sources).unwrap().1, cache_root);
        // ... after Xcode's, after $SCRIOS_DDI_DIR, after --ddi-dir.
        let sources = DdiSources { xcode: vec![xcode.path().into()], ..sources };
        assert_eq!(find(&sources).unwrap(), (Origin::Xcode, xcode.path().to_path_buf()));
        let sources = DdiSources { env_dir: Some(env.path().into()), ..sources };
        assert_eq!(find(&sources).unwrap(), (Origin::Env, env.path().to_path_buf()));
        let sources = DdiSources { dir: Some(flag.path().into()), ..sources };
        assert_eq!(find(&sources).unwrap(), (Origin::Option, flag.path().to_path_buf()));
        assert_eq!(fake.asked().len(), 5, "only the first obtain downloaded");
    }

    /// For a person: `SCRIOS_TEST_CRYPTEX_DIR=<a real DDI folder> cargo test --lib real_cryptex_folder -- --ignored`
    /// (the folder `scrios ddi-fetch` fills, or Xcode's `Restore`). Reads the files; touches no device.
    #[tokio::test]
    #[ignore = "needs a real DDI folder in $SCRIOS_TEST_CRYPTEX_DIR"]
    async fn a_real_cryptex_folder_loads_the_way_cryptexd_install_reads_it() {
        let dir = std::env::var("SCRIOS_TEST_CRYPTEX_DIR").expect("SCRIOS_TEST_CRYPTEX_DIR");
        let folder = DdiFolder::open(Path::new(&dir)).unwrap();
        folder.check(Variant::Cryptex).unwrap();
        let assets = folder.cryptex_assets().await.unwrap();
        let properties = assets.cryptex1_properties().unwrap();
        println!(
            "build {:?}: image {} bytes, trust cache {}, info {}, volume hash {}, nonce domain {}, {} properties",
            folder.build(),
            assets.image.len(),
            assets.trustcache.len(),
            assets.info.len(),
            assets.volumehash.len(),
            assets.nonce_domain().unwrap(),
            properties.len()
        );
        assert!(!assets.image.is_empty() && !assets.trustcache.is_empty() && !assets.volumehash.is_empty());
    }
}
