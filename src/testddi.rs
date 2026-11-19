//! Folders of the developer disk image for the tests: a cryptex folder (what `cryptexd` installs) and a personalized
//! one (what the image mounter mounts), small enough to write for each test, with the shape of the real ones.

use std::sync::Arc;

use crate::ddi::fetch::Mirror;
use crate::ddi::manifest::Manifest;
use crate::ddi::{DdiMode, DdiSources, Remote, sha256_hex};
use crate::testdir::TempDir;
use crate::testnet::FakeFetch;

/// The payloads of the cryptex folder, by the name of the file that holds each.
pub(crate) const CRYPTEX_IMAGE: &[u8] = b"cryptex image";
pub(crate) const CRYPTEX_TRUSTCACHE: &[u8] = b"cryptex trust cache";
pub(crate) const CRYPTEX_INFO: &[u8] = b"cryptex info";
pub(crate) const CRYPTEX_VOLUME: &[u8] = b"cryptex volume hash";
/// The `Cryptex1,NonceDomain` of the cryptex identity.
pub(crate) const NONCE_DOMAIN: u64 = 4;

fn dict(pairs: Vec<(&str, plist::Value)>) -> plist::Value {
    plist::Value::Dictionary(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

/// A manifest entry for the file at `path`; one that is personalized (signed into the ticket) has a digest, as in the
/// real manifest.
fn info(path: &str, personalize: bool) -> plist::Value {
    let mut entry = vec![("Info", dict(vec![("Path", path.into()), ("Personalize", personalize.into())]))];
    if personalize {
        entry.push(("Digest", plist::Value::Data(vec![7; 48])));
    }
    dict(entry)
}

fn xml(top: &plist::Value) -> Vec<u8> {
    let mut bytes = Vec::new();
    plist::to_writer_xml(&mut bytes, top).unwrap();
    bytes
}

/// A build manifest with a personalized identity (an iPhone's) that names `image` and `trust_cache`, and, if `cryptex`
/// is `Some((image, trust cache, info, volume))`, a cryptex identity (the way the real manifest has one, with the
/// types the real one has: the nonce domain is an unsigned integer, the chip ID a `0x` string).
pub(crate) fn manifest(personalized: Option<(&str, &str)>, cryptex: Option<[&str; 4]>) -> Vec<u8> {
    let mut identities = Vec::new();
    if let Some((image, trust_cache)) = personalized {
        identities.push(dict(vec![
            ("ApBoardID", "0x0A".into()),
            ("ApChipID", "0x8110".into()),
            ("Info", dict(vec![("Variant", "Customer iOS Developer PDI".into())])),
            (
                "Manifest",
                dict(vec![("PersonalizedDMG", info(image, false)), ("LoadableTrustCache", info(trust_cache, true))]),
            ),
        ]));
    }
    if let Some([image, trust_cache, cryptex_info, volume]) = cryptex {
        identities.push(dict(vec![
            ("Info", dict(vec![("Variant", "iOS Customer Developer Disk Image Cryptex".into())])),
            ("Cryptex1,ChipID", "0xFF10".into()),
            ("Cryptex1,ProductClass", "0xF2".into()),
            ("Cryptex1,Type", 3_u64.into()),
            ("Cryptex1,SubType", 2_u64.into()),
            ("Cryptex1,NonceDomain", NONCE_DOMAIN.into()),
            ("Cryptex1,UseProductClass", true.into()),
            ("Cryptex1,Version", "39.999.999.0.0,0".into()),
            ("Cryptex1,PreauthorizationVersion", "39.999.999.0.0,0".into()),
            (
                "Manifest",
                dict(vec![
                    ("Cryptex1,GenericDmg", info(image, false)),
                    ("Cryptex1,GenericTrustCache", info(trust_cache, true)),
                    ("Cryptex1,CryptexInfoPlist", info(cryptex_info, true)),
                    ("Cryptex1,GenericVolume", info(volume, true)),
                ]),
            ),
        ]));
    }
    xml(&dict(vec![("ProductBuildVersion", "27B52".into()), ("BuildIdentities", plist::Value::Array(identities))]))
}

/// The files of the cryptex folder of the `DeveloperDiskImage` repository (fixed names), in `dir`.
pub(crate) fn cryptex_folder(dir: &TempDir) {
    dir.write(
        "BuildManifest.plist",
        &manifest(None, Some(["Image.dmg", "Image.dmg.trustcache", "Image.dmg.cryptex_info", "Image.dmg.root_hash"])),
    );
    dir.write("Image.dmg", CRYPTEX_IMAGE);
    dir.write("Image.dmg.trustcache", CRYPTEX_TRUSTCACHE);
    dir.write("Image.dmg.cryptex_info", CRYPTEX_INFO);
    dir.write("Image.dmg.root_hash", CRYPTEX_VOLUME);
}

/// The files of the personalized folder of the repository (fixed names), in `dir`.
pub(crate) fn personalized_folder(dir: &TempDir) {
    dir.write("BuildManifest.plist", &manifest(Some(("Image.dmg", "Image.dmg.trustcache")), None));
    dir.write("Image.dmg", b"personalized image");
    dir.write("Image.dmg.trustcache", b"personalized trust cache");
}

/// The content of the cryptex folder, as a mirror would serve it: `(path in the repository, bytes)`.
fn mirror_content() -> Vec<(String, Vec<u8>)> {
    let dir = TempDir::new("ddi-mirror-content");
    cryptex_folder(&dir);
    ["BuildManifest.plist", "Image.dmg", "Image.dmg.trustcache", "Image.dmg.cryptex_info", "Image.dmg.root_hash"]
        .iter()
        .map(|name| {
            (format!("PersonalizedImages/Xcode_iOS_DDI_Cryptex/{name}"), std::fs::read(dir.path().join(name)).unwrap())
        })
        .collect()
}

/// Sources that download the cryptex (`DdiMode::Auto`) from a mirror at `https://mirror.example/ddi`, revision
/// `abc123`, that a fake serves, into a cache of their own (which the third value keeps alive). Nothing else is
/// found locally.
pub(crate) fn mirror_world() -> (DdiSources, Arc<FakeFetch>, TempDir) {
    let content = mirror_content();
    let manifest: String =
        content.iter().map(|(path, bytes)| format!("{}  {path}\n", sha256_hex(bytes))).collect::<Vec<_>>().concat();
    let fake = Arc::new(FakeFetch::default());
    for (path, bytes) in &content {
        fake.answer(&format!("https://mirror.example/ddi/abc123/{path}"), Ok(bytes.clone()));
    }
    let mirror = Mirror::new("https://mirror.example/ddi", "abc123", Manifest::parse(&manifest).unwrap()).unwrap();
    let cache = TempDir::new("ddi-mirror-cache");
    let sources = DdiSources {
        cache: Some(cache.path().join("ddi")),
        mode: DdiMode::Auto,
        remote: Remote::Mirror(mirror),
        fetcher: Some(fake.clone()),
        ..DdiSources::default()
    };
    (sources, fake, cache)
}
