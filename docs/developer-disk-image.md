# The developer disk image

An iOS device only offers its screen to developer tools after Apple's developer disk image is installed on it.
[Xcode](https://developer.apple.com/xcode/) does this for you. So does scrios.

You normally do not need to do anything. This page is for when you want to control where the image comes from.

## Where scrios looks

In this order:

1. The folder given with `--ddi-dir`.
2. Xcode's copy, on a Mac that has Xcode.
3. The scrios cache folder.
4. A download.

The cache folder is `~/Library/Caches/scrios/ddi` on macOS, `~/.cache/scrios/ddi` on Linux and
`%LOCALAPPDATA%\scrios\ddi` on Windows.

## The download

The image is Apple's, and scrios does not host it. It downloads the files from
[doronz88/DeveloperDiskImage](https://github.com/doronz88/DeveloperDiskImage), a repository run by the
[pymobiledevice3](https://github.com/doronz88/pymobiledevice3) project that republishes the image Xcode ships.

- The download is about 18 MB and is kept in the cache folder.
- It is pinned to one commit of that repository.
- Every file is checked against a SHA-256 built into scrios. A file that does not match is thrown away.

## Installing

Installing needs internet access, because Apple's signing server has to approve the image for your device. This is
needed again after the device restarts.

On iOS 17.4 and later the image is installed the way Xcode does it (through `cryptexd`). Older versions use the image
mounter. scrios picks the right one.

## Options

| Option | What it does |
| --- | --- |
| `--ddi-source auto` | Use a local copy, or download one. This is the default. |
| `--ddi-source local` | Never download. |
| `--ddi-source none` | Never install anything. |
| `--ddi-dir DIR` | Use this folder. It is the `iOS_DDI` folder from a Mac with Xcode. |

Each option has an environment variable: `SCRIOS_DDI_SOURCE` and `SCRIOS_DDI_DIR`.

## Without internet access

Fill the cache on a machine that has internet, then copy the cache folder across:

```
scrios ddi-fetch
```

Or copy the image from a Mac with Xcode:

```
scp -r mac:/Library/Developer/DeveloperDiskImages/iOS_DDI ~/.cache/scrios/ddi
```

Then run scrios with `--ddi-source local`. Installing the image on the device still needs to reach Apple's signing
server.

## Behind a corporate proxy

If your network inspects HTTPS traffic, the download fails with an `UnknownIssuer` error until scrios trusts the
proxy's certificate. scrios trusts whatever your system trusts, so either install the certificate in the system trust
store or point to it:

```
SSL_CERT_FILE=/path/to/ca.pem scrios
```

scrios does not use `HTTPS_PROXY`. If the network only allows traffic through a proxy, use a local copy as described
above.

## Hosting the image yourself

To download from your own server instead of GitHub, use one of these:

| Options | What you host |
| --- | --- |
| `--ddi-repo URL --ddi-ref COMMIT --ddi-sha256-manifest FILE` | A mirror of the repository. The manifest is a `sha256sum` list of its files. |
| `--ddi-download-url URL --ddi-sha256 HEX` | A `.tar.gz` of the `iOS_DDI` folder. |

Both need the checksums. scrios does not use a download it cannot verify.

To make the manifest for a mirror:

```
cd DeveloperDiskImage
sha256sum PersonalizedImages/Xcode_iOS_DDI_Cryptex/* PersonalizedImages/Xcode_iOS_DDI_Personalized/* > ddi.sha256
```

## Commands

| Command | What it does |
| --- | --- |
| `scrios ddi-status` | Show whether the image is installed and where scrios would get it. |
| `scrios mount-ddi` | Install it now. |
| `scrios unmount-ddi` | Remove it. |
| `scrios ddi-fetch` | Download it into the cache without a device. `--variant all` fetches both forms. |

Updating the pinned commit is described in [BUILD.md](../BUILD.md#updating-the-developer-disk-image).
