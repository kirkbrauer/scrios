//! The developer disk image that scrios downloads when it finds none: one commit of the `DeveloperDiskImage`
//! repository of the pymobiledevice3 project, and the size and SHA-256 of each file in it that scrios uses.
//!
//! The files are Apple's: the repository publishes the developer disk image that Xcode installs (the `iOS_DDI` folder
//! of Xcode), cut into files that a program on any host can use. scrios does not host them and does not own them; it
//! downloads them from GitHub's raw file service, the commit named here, over https, and uses a file only if its size
//! and SHA-256 are the ones below. A commit is immutable: what is downloaded cannot change under the pin, and a
//! download that is not what was pinned (a server that is not GitHub, a damaged file, a repository that was rewritten)
//! is refused before it is written to the cache or sent to a device.
//!
//! Updating the pin is a deliberate change of this file (and nothing else), when a newer Xcode's image is wanted
//! (a device on a newer iOS wants a newer image):
//!
//! 1. find the commit that brought the build: `gh api 'repos/doronz88/DeveloperDiskImage/commits?path=PersonalizedImages/Xcode_iOS_DDI_Cryptex&per_page=3'`
//!    (the newest commit that touched the folder; its message says `Update DDIs to <build>`);
//! 2. download the files of that commit into an empty folder (the URLs are `https://raw.githubusercontent.com/<repo>/<commit>/PersonalizedImages/<folder>/<name>`);
//!    check each against the repository's own blob hashes (`gh api 'repos/<repo>/contents/PersonalizedImages/<folder>?ref=<commit>'`
//!    lists their git `sha`; `git hash-object <file>` must print the same);
//! 3. put the commit, the build (`ProductBuildVersion` in `BuildManifest.plist`), and each file's size (bytes) and
//!    `shasum -a 256` into the tables below;
//! 4. run `scrios ddi-fetch --variant all` with an empty cache (`XDG_CACHE_HOME=$(mktemp -d)`): it downloads, verifies
//!    and loads the files; `.github/workflows/ddi-pin.yml` does the same every week, and says when the repository has
//!    a newer commit than the pin.

use super::Variant;

/// The repository (`owner/name`) the files are downloaded from.
pub const REPO: &str = "doronz88/DeveloperDiskImage";
/// What a message calls it.
pub const SOURCE: &str = "github.com/doronz88/DeveloperDiskImage";
/// GitHub's raw file service, which serves the files of a commit.
pub const HOST: &str = "raw.githubusercontent.com";
/// The commit the files are pinned to ("maintenance: Update DDIs to `27B52`", 2026-11-11).
pub const COMMIT: &str = "dcec53989fa57695c2b3d3e11c2585f4735b2fef";
/// The Xcode build the image is of (`ProductBuildVersion` of its manifest): it names the cache folder.
pub const BUILD: &str = "27B52";
/// The folder of the repository that holds a folder for each variant.
pub const IMAGES_DIR: &str = "PersonalizedImages";

/// A file of the pin: its name in the folder, its size and the SHA-256 of its content (lowercase hex).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinnedFile {
    pub name: &'static str,
    pub size: u64,
    pub sha256: &'static str,
}

/// `PersonalizedImages/Xcode_iOS_DDI_Cryptex`: what `cryptexd` installs (iOS 17.4 and later). The folder is a drop-in
/// `Restore` folder: its manifest names the files by these names.
pub const CRYPTEX_FILES: [PinnedFile; 5] = [
    PinnedFile {
        name: "BuildManifest.plist",
        size: 804_097,
        sha256: "537391eb2f37e604fc693f422c1568af97220067853e663391ba3e8258569d6e",
    },
    PinnedFile {
        name: "Image.dmg",
        size: 17_239_040,
        sha256: "500d4139f7963650e6c796d950267ed38909e7d183ad26bba073257529bfe173",
    },
    PinnedFile {
        name: "Image.dmg.trustcache",
        size: 1_851,
        sha256: "2719aa3f1c99245432512116fb8a56d55ec224c706e545772d4c17c1233ac475",
    },
    PinnedFile {
        name: "Image.dmg.cryptex_info",
        size: 428,
        sha256: "0180ee20ce5daa7978fe38f24c742361726789ea6af82c3c1d2afc480734a531",
    },
    PinnedFile {
        name: "Image.dmg.root_hash",
        size: 229,
        sha256: "5f3281961396be5fe008e11641ffc25c7db24c11d3789ff0c4ff0fa6d47b94ca",
    },
];

/// `PersonalizedImages/Xcode_iOS_DDI_Personalized`: what the image mounter mounts (iOS 17.0 to 17.3.1, and where
/// `cryptexd` is not offered).
pub const PERSONALIZED_FILES: [PinnedFile; 3] = [
    PinnedFile {
        name: "BuildManifest.plist",
        size: 800_656,
        sha256: "3c4eee2be4a0a853044dc32dbbd62699302b1dfd317aaf4f1cf36368b7639bb0",
    },
    PinnedFile {
        name: "Image.dmg",
        size: 17_084_416,
        sha256: "0de55b0ccde9d5790c9c9f4e1a413225443fce7ea75acb503aca11e7d8156646",
    },
    PinnedFile {
        name: "Image.dmg.trustcache",
        size: 1_851,
        sha256: "f0fb56da2829f69b8aae113584349635738e68d77c82a726ed72040890f16bf4",
    },
];

/// The pinned files of `variant`.
#[must_use]
pub fn files(variant: Variant) -> &'static [PinnedFile] {
    match variant {
        Variant::Cryptex => &CRYPTEX_FILES,
        Variant::Personalized => &PERSONALIZED_FILES,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pin_is_a_commit_a_build_and_files_with_sizes_and_hashes_that_look_right() {
        assert!(COMMIT.len() == 40 && COMMIT.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')), "{COMMIT}");
        assert!(!BUILD.is_empty() && BUILD.bytes().all(|b| b.is_ascii_alphanumeric()), "{BUILD}");
        assert!(SOURCE.ends_with(REPO));
        for variant in [Variant::Cryptex, Variant::Personalized] {
            let files = files(variant);
            assert!(files.iter().any(|f| f.name == "BuildManifest.plist" && f.size > 0), "{variant:?}");
            for (index, file) in files.iter().enumerate() {
                assert!(
                    file.sha256.len() == 64 && file.sha256.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
                    "{}: {}",
                    file.name,
                    file.sha256
                );
                assert!(file.size > 0 && file.size <= super::super::fetch::MAX_FILE_BYTES, "{}", file.name);
                // A plain name in the folder, and each one once.
                assert!(
                    crate::ddi::safe_relative(file.name).is_ok_and(|p| p.components().count() == 1),
                    "{}",
                    file.name
                );
                assert!(files.iter().skip(index + 1).all(|other| other.name != file.name), "{}", file.name);
            }
        }
        // The cryptex folder is what idevice reads: the image, trust cache, cryptex info and volume hash by their names.
        let names: Vec<&str> = CRYPTEX_FILES.iter().map(|f| f.name).collect();
        for want in ["Image.dmg", "Image.dmg.trustcache", "Image.dmg.cryptex_info", "Image.dmg.root_hash"] {
            assert!(names.contains(&want), "{want}");
        }
    }
}
