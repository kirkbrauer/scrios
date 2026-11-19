//! The developer disk image (DDI): where scrios finds one to mount, and which of its files belong to a device.
//!
//! The display service is only offered to a device that has the developer disk image mounted. iOS 17 and later take
//! the *personalized* image: a folder of the kind Xcode installs (`iOS_DDI`), with a `BuildManifest.plist`, the
//! image (`.dmg`) and its trust cache, which Apple's signing server (TSS) personalizes for the device's chip.
//!
//! scrios never ships or hosts Apple's image, and it has no default place to download one from. It looks, in this
//! order, at:
//!
//! 1. the folder given (`--ddi-dir`),
//! 2. `$SCRIOS_DDI_DIR`,
//! 3. on macOS, the folders Xcode installs the image in,
//! 4. a per-user cache folder (see [`cache_dir`]), where a folder copied from a Mac with Xcode is kept, and where a
//!    download goes if the caller configured one ([`Download`]: a URL and the SHA-256 it must match).
//!
//! In a folder, the build manifest is looked for in the folder, in `Restore/`, and under an `iOS_DDI/` folder in
//! either. The image and trust cache are the ones the manifest names for the device's board and chip (Xcode's layout:
//! `Restore/<name>.dmg` and `Restore/Firmware/<name>.dmg.trustcache`), else `Image4.dmg`, `Image.dmg` or
//! `DeveloperDiskImage.dmg` with a `.trustcache` next to it.

use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tracing::{debug, info};

/// The environment variable that names a DDI folder.
pub const ENV_DIR: &str = "SCRIOS_DDI_DIR";
/// The environment variable that names the URL of a DDI archive to download.
pub const ENV_URL: &str = "SCRIOS_DDI_URL";
/// The environment variable that holds the SHA-256 of the archive at [`ENV_URL`].
pub const ENV_SHA256: &str = "SCRIOS_DDI_SHA256";

/// The largest archive that is unpacked (its entries' sizes added up): the image itself is some tens of MB.
const MAX_UNPACKED: u64 = 2 * 1024 * 1024 * 1024;
/// The manifest's file name.
const MANIFEST: &str = "BuildManifest.plist";
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

/// Where to look for a DDI folder, in the order of the module's documentation.
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
}

/// A DDI folder that was found, and where.
#[derive(Debug, Clone)]
pub struct Found {
    pub folder: DdiFolder,
    pub origin: Origin,
}

impl DdiSources {
    /// The sources on this host: `dir` and `url` and `sha256` are the options, the rest is read from the
    /// environment ([`ENV_DIR`], and [`ENV_URL`] with [`ENV_SHA256`] for what the options leave out) and the host.
    ///
    /// # Errors
    ///
    /// A message if only one of the URL and the hash is given, or either is malformed.
    pub fn from_host(dir: Option<PathBuf>, url: Option<&str>, sha256: Option<&str>) -> Result<DdiSources, String> {
        let env = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        let mut sources = DdiSources::for_platform(
            Platform::current(),
            dir,
            env(ENV_DIR).map(PathBuf::from),
            url.map(str::to_string).or_else(|| env(ENV_URL)),
            sha256.map(str::to_string).or_else(|| env(ENV_SHA256)),
            &|name| std::env::var(name).ok(),
        )?;
        sources.xcode = xcode_dirs();
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
        Ok(DdiSources { dir, env_dir, xcode: Vec::new(), cache: cache_dir_for(platform, get), download })
    }

    /// The first DDI folder there is. A folder that was asked for (`--ddi-dir`, `$SCRIOS_DDI_DIR`) must be a DDI
    /// folder; Xcode's and the cache are tried only if they exist.
    ///
    /// # Errors
    ///
    /// The reason an explicit folder is not a DDI folder, or [`DdiSources::not_found`] if there is none to be found.
    pub fn locate(&self) -> Result<Found, String> {
        if let Some(dir) = &self.dir {
            return DdiFolder::open(dir)
                .map(|folder| Found { folder, origin: Origin::Option })
                .map_err(|why| format!("--ddi-dir: {why}"));
        }
        if let Some(dir) = &self.env_dir {
            return DdiFolder::open(dir)
                .map(|folder| Found { folder, origin: Origin::Env })
                .map_err(|why| format!("${ENV_DIR}: {why}"));
        }
        let tried =
            self.xcode.iter().map(|dir| (dir, Origin::Xcode)).chain(self.cache.iter().map(|dir| (dir, Origin::Cache)));
        for (dir, origin) in tried {
            match DdiFolder::open(dir) {
                Ok(folder) => return Ok(Found { folder, origin }),
                Err(why) => debug!("no DDI in {}: {why}", dir.display()),
            }
        }
        Err(self.not_found())
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
        format!(
            "no developer disk image (DDI) found (looked in {}). scrios does not ship Apple's image and has no default place to download it from: copy the iOS_DDI folder from a Mac with Xcode (/Library/Developer/DeveloperDiskImages/iOS_DDI) into {cache} so that {manifest} exists, or point --ddi-dir (or ${ENV_DIR}) at it, or give --ddi-download-url with --ddi-sha256 for an archive of it you host",
            looked.join(", ")
        )
    }

    /// [`DdiSources::locate`], and if there is none and a download is configured, the download into the cache
    /// (`downloading` is told the host it is downloaded from first: the rest of the URL can carry a secret).
    ///
    /// # Errors
    ///
    /// The reason there is no usable DDI folder: what [`DdiSources::locate`] says, or why the download failed.
    pub async fn find_or_download(&self, downloading: &mut (dyn FnMut(&str) + Send)) -> Result<Found, String> {
        let first = self.locate();
        let (Err(why), Some(download)) = (&first, &self.download) else { return first };
        let Some(cache) = &self.cache else {
            return Err(format!(
                "{why} (and there is no cache folder to download into: HOME or LOCALAPPDATA is not set)"
            ));
        };
        if self.dir.is_some() || self.env_dir.is_some() {
            return first; // an explicit folder that does not work is not replaced by a download
        }
        downloading(&host_of(&download.url));
        let folder = Box::pin(fetch(download, cache)).await?;
        Ok(Found { folder, origin: Origin::Cache })
    }
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
            download: None,
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
            ..DdiSources::default()
        };
        let mut said = Vec::new();
        let found = Box::pin(sources(sha256_hex(&archive)).find_or_download(&mut |line| said.push(line.to_string())))
            .await
            .unwrap();
        assert_eq!(found.origin, Origin::Cache);
        assert_eq!(found.folder.root(), dest.join("Restore"));
        assert_eq!(said, vec![format!("127.0.0.1:{port}")], "only the host is said");
        // The wrong hash is refused, and the cache keeps nothing of it.
        let _ = std::fs::remove_dir_all(&dest);
        let why = Box::pin(sources("0".repeat(64)).find_or_download(&mut |_| {})).await.unwrap_err();
        assert!(why.contains("not used"), "{why}");
        assert!(!dest.exists());
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
}
