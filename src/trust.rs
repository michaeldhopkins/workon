//! Config trust gate.
//!
//! A workon config is a Zellij layout, and a layout can launch arbitrary
//! commands in its panes (`pane command="bash" { args "-c" "rm -rf ~" }`).
//! Anything that can drop a `.kdl` into `~/.config/workon/configs/` could
//! therefore get code to run the next time that config is launched. To close
//! that, workon runs an on-disk config only when the user has blessed it by hand
//! in `~/.config/workon/trusted.toml` — a `path` + `sha256` pin per file. Any
//! later edit changes the file's hash and un-trusts it until re-reviewed.
//!
//! The trust list is the root of trust: workon never writes it, and there is no
//! `workon trust` subcommand (that would be agent-invocable, defeating the
//! point). The boundary holds only while the manifest stays outside an agent's
//! no-permission write scope — the same assumption safe-chains makes about
//! `~/.config/safe-chains.toml`. The embedded default layout is compiled into
//! the binary, has no file to tamper with, and is never gated.

use std::path::Path;

use anyhow::{anyhow, bail, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const TRUST_FILENAME: &str = "trusted.toml";

const TRUSTED_CONFIGS_URL: &str = "https://github.com/michaeldhopkins/workon#trusting-configs";

#[derive(Deserialize, Default)]
struct TrustManifest {
    #[serde(default)]
    trusted: Vec<TrustedEntry>,
}

#[derive(Deserialize)]
struct TrustedEntry {
    path: String,
    sha256: String,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Read `~/.config/workon/trusted.toml` (under `workon_dir`). A missing file
/// means "nothing trusted"; a malformed one is reported and also treated as
/// empty, so workon fails closed rather than honoring an unparseable list.
fn load_manifest(workon_dir: &Path) -> TrustManifest {
    let path = workon_dir.join(TRUST_FILENAME);
    let source = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return TrustManifest::default(),
    };
    match toml::from_str(&source) {
        Ok(manifest) => manifest,
        Err(e) => {
            eprintln!("workon: ignoring malformed {}: {e}", path.display());
            TrustManifest::default()
        }
    }
}

/// A config is trusted when some `[[trusted]]` entry pins both its canonical
/// path and its content hash. Canonicalizing both sides keeps the comparison
/// honest across symlinks and `..` segments; an entry whose `path` no longer
/// resolves simply doesn't match.
fn is_trusted(path: &Path, hash: &str, manifest: &TrustManifest) -> bool {
    let Ok(canon) = std::fs::canonicalize(path) else {
        return false;
    };
    manifest.trusted.iter().any(|entry| {
        entry.sha256.trim().eq_ignore_ascii_case(hash)
            && std::fs::canonicalize(&entry.path).map(|p| p == canon).unwrap_or(false)
    })
}

/// Read a config file only if it is blessed in `<workon_dir>/trusted.toml`,
/// otherwise bail with copy-pasteable instructions for trusting it.
pub fn read_trusted(workon_dir: &Path, path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)?;
    let hash = sha256_hex(&bytes);
    let manifest = load_manifest(workon_dir);
    if !is_trusted(path, &hash, &manifest) {
        bail!("{}", untrusted_message(workon_dir, path, &hash));
    }
    String::from_utf8(bytes).map_err(|_| anyhow!("config {} is not valid UTF-8", path.display()))
}

fn untrusted_message(workon_dir: &Path, path: &Path, hash: &str) -> String {
    // Show the canonical path so the pasted `path` matches what `is_trusted`
    // will compare against at load time.
    let display = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let manifest = workon_dir.join(TRUST_FILENAME);
    format!(
        "refusing to run an untrusted config:\n\
         \n  {display}\n  sha256: {hash}\n\n\
         A config can launch arbitrary commands in its panes, so workon only runs\n\
         configs you have blessed by hand. To trust this one, add to {manifest}:\n\n\
         [[trusted]]\n\
         path = \"{display}\"\n\
         sha256 = \"{hash}\"\n\n\
         Edit that file yourself — workon never writes it. Update the sha256\n\
         whenever the config changes.\n\n\
         See: {TRUSTED_CONFIGS_URL}",
        display = display.display(),
        manifest = manifest.display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::{Path, PathBuf};

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    /// Append a `[[trusted]]` pin for `path` (with the hash of `body`) to
    /// `<workon_dir>/trusted.toml`, mirroring what a user does by hand.
    fn bless(workon_dir: &Path, path: &Path, body: &str) {
        let canon = std::fs::canonicalize(path).unwrap();
        let entry = format!(
            "[[trusted]]\npath = {:?}\nsha256 = \"{}\"\n",
            canon.to_string_lossy(),
            sha256_hex(body.as_bytes()),
        );
        let manifest = workon_dir.join(TRUST_FILENAME);
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&manifest)
            .unwrap();
        f.write_all(entry.as_bytes()).unwrap();
    }

    #[test]
    fn sha256_hex_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn read_trusted_succeeds_when_blessed() {
        let dir = tempfile::tempdir().unwrap();
        let body = "pane command=\"claude\"\n";
        let path = write(dir.path(), "default.kdl", body);
        bless(dir.path(), &path, body);

        assert_eq!(read_trusted(dir.path(), &path).unwrap(), body);
    }

    #[test]
    fn read_trusted_refuses_when_not_listed() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "default.kdl", "pane command=\"claude\"\n");

        let err = read_trusted(dir.path(), &path).unwrap_err().to_string();
        assert!(err.contains("untrusted config"), "{err}");
        assert!(err.contains("[[trusted]]"), "{err}");
        assert!(err.contains("trusted.toml"), "{err}");
        assert!(err.contains("sha256"), "{err}");
    }

    #[test]
    fn read_trusted_rejects_non_utf8_even_when_blessed() {
        let dir = tempfile::tempdir().unwrap();
        let raw = b"layout {\n\xff\xfe pane\n}\n"; // 0xff/0xfe are invalid UTF-8
        let path = dir.path().join("default.kdl");
        std::fs::write(&path, raw).unwrap();

        // Pin the file's exact bytes so it clears the trust gate; the failure
        // must then come from UTF-8 decoding, not from being untrusted.
        let canon = std::fs::canonicalize(&path).unwrap();
        let manifest = format!(
            "[[trusted]]\npath = {:?}\nsha256 = \"{}\"\n",
            canon.to_string_lossy(),
            sha256_hex(raw),
        );
        std::fs::write(dir.path().join(TRUST_FILENAME), manifest).unwrap();

        let err = read_trusted(dir.path(), &path).unwrap_err().to_string();
        assert!(err.contains("not valid UTF-8"), "{err}");
        assert!(!err.contains("untrusted config"), "{err}");
    }

    #[test]
    fn read_trusted_refuses_after_tampering() {
        let dir = tempfile::tempdir().unwrap();
        let body = "pane command=\"claude\"\n";
        let path = write(dir.path(), "default.kdl", body);
        bless(dir.path(), &path, body);

        // An attacker rewrites the blessed file with a destructive command.
        std::fs::write(&path, "pane command=\"bash\" { args \"-c\" \"rm -rf ~\" }\n").unwrap();

        let err = read_trusted(dir.path(), &path).unwrap_err().to_string();
        assert!(err.contains("untrusted config"), "{err}");
    }

    #[test]
    fn is_trusted_requires_matching_path() {
        let dir = tempfile::tempdir().unwrap();
        let body = "pane command=\"claude\"\n";
        let path = write(dir.path(), "default.kdl", body);
        let manifest = TrustManifest {
            trusted: vec![TrustedEntry {
                path: "/some/other/place.kdl".to_string(),
                sha256: sha256_hex(body.as_bytes()),
            }],
        };
        // Right hash, wrong path: not trusted.
        assert!(!is_trusted(&path, &sha256_hex(body.as_bytes()), &manifest));
    }

    #[test]
    fn is_trusted_will_not_combine_path_and_hash_across_entries() {
        // The path from one entry must not pair with the hash from another:
        // both have to match within a single `[[trusted]]`.
        let dir = tempfile::tempdir().unwrap();
        let body = "pane command=\"claude\"\n";
        let path = write(dir.path(), "default.kdl", body);
        let canon = std::fs::canonicalize(&path).unwrap();
        let manifest = TrustManifest {
            trusted: vec![
                // Right path, wrong hash.
                TrustedEntry {
                    path: canon.to_string_lossy().into_owned(),
                    sha256: sha256_hex(b"something else"),
                },
                // Right hash, wrong path.
                TrustedEntry {
                    path: "/some/other/place.kdl".to_string(),
                    sha256: sha256_hex(body.as_bytes()),
                },
            ],
        };
        assert!(!is_trusted(&path, &sha256_hex(body.as_bytes()), &manifest));
    }

    #[test]
    fn is_trusted_is_case_insensitive_on_hash() {
        let dir = tempfile::tempdir().unwrap();
        let body = "pane command=\"claude\"\n";
        let path = write(dir.path(), "default.kdl", body);
        let canon = std::fs::canonicalize(&path).unwrap();
        let manifest = TrustManifest {
            trusted: vec![TrustedEntry {
                path: canon.to_string_lossy().into_owned(),
                sha256: sha256_hex(body.as_bytes()).to_uppercase(),
            }],
        };
        assert!(is_trusted(&path, &sha256_hex(body.as_bytes()), &manifest));
    }

    #[test]
    fn malformed_manifest_trusts_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let body = "pane command=\"claude\"\n";
        let path = write(dir.path(), "default.kdl", body);
        std::fs::write(dir.path().join(TRUST_FILENAME), "not valid toml {{{").unwrap();

        assert!(read_trusted(dir.path(), &path).is_err());
    }

    #[test]
    fn missing_manifest_trusts_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "default.kdl", "pane command=\"claude\"\n");
        assert!(read_trusted(dir.path(), &path).is_err());
    }
}
