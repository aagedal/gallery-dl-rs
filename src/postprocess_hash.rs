//! Bounded, cancellable streaming file digests for native hash hooks.
use crate::{Result, error};
use serde_json::{Map, Value};
use sha2::digest::{Digest, DynDigest};
use std::{
    fs::{File, OpenOptions},
    io::Read,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
const MAX_CHUNK: usize = 16 * 1024 * 1024;
const MAX_HASHES: usize = 128;

#[derive(Clone)]
pub struct HashOptions {
    hashes: Vec<(String, String)>,
    chunk: usize,
    filename: bool,
}
impl std::fmt::Debug for HashOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HashOptions { configuration: [redacted] }")
    }
}
fn truth(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(v) => *v,
        Value::Number(v) => v.as_f64() != Some(0.0),
        Value::String(v) => !v.is_empty(),
        Value::Array(v) => !v.is_empty(),
        Value::Object(v) => !v.is_empty(),
    }
}
fn digest(name: &str) -> Result<Box<dyn DynDigest + Send>> {
    // BLAKE2 is provided directly by hashlib rather than its case-insensitive
    // OpenSSL registry. Preserve that distinction for its canonical names.
    if name == "blake2b" {
        return Ok(Box::new(blake2::Blake2b512::new()));
    }
    if name == "blake2s" {
        return Ok(Box::new(blake2::Blake2s256::new()));
    }
    let lower = name.to_ascii_lowercase();
    Ok(match lower.as_str() {
        "md5" => Box::new(md5::Md5::new()),
        "sha1" | "sha-1" => Box::new(sha1::Sha1::new()),
        "sha224" | "sha-224" => Box::new(sha2::Sha224::new()),
        "sha256" | "sha-256" => Box::new(sha2::Sha256::new()),
        "sha384" | "sha-384" => Box::new(sha2::Sha384::new()),
        "sha512" | "sha-512" => Box::new(sha2::Sha512::new()),
        "sha512_224" | "sha512-224" => Box::new(sha2::Sha512_224::new()),
        "sha512_256" | "sha512-256" => Box::new(sha2::Sha512_256::new()),
        "sha3_224" | "sha3-224" => Box::new(sha3::Sha3_224::new()),
        "sha3_256" | "sha3-256" => Box::new(sha3::Sha3_256::new()),
        "sha3_384" | "sha3-384" => Box::new(sha3::Sha3_384::new()),
        "sha3_512" | "sha3-512" => Box::new(sha3::Sha3_512::new()),
        _ => return Err(error("unsupported native hash algorithm")),
    })
}
impl HashOptions {
    pub fn parse(options: &Map<String, Value>) -> Result<Self> {
        let hashes = options
            .get("mode")
            .filter(|v| truth(v))
            .or_else(|| options.get("hashes"));
        let mut pairs = Vec::new();
        match hashes {
            None | Some(Value::Null | Value::Bool(false)) => {
                pairs.extend([("md5".into(), "md5".into()), ("sha1".into(), "sha1".into())])
            }
            Some(value) if !truth(value) && !value.is_object() && !value.is_string() => {
                pairs.extend([("md5".into(), "md5".into()), ("sha1".into(), "sha1".into())])
            }
            Some(Value::Object(values)) => {
                if values.len() > MAX_HASHES {
                    return Err(error("hash options exceed supported limits"));
                }
                for (key, algorithm) in values {
                    pairs.push((
                        key.clone(),
                        algorithm
                            .as_str()
                            .ok_or_else(|| error("invalid hash algorithm configuration"))?
                            .into(),
                    ));
                }
            }
            Some(Value::String(values)) => {
                for value in values.split(',').take(MAX_HASHES + 1) {
                    let (algorithm, key) = value
                        .split_once(':')
                        .map_or((value, value), |(a, k)| (a, k));
                    pairs.push((key.into(), algorithm.into()));
                }
            }
            Some(Value::Array(values)) if values.len() <= MAX_HASHES => {
                for value in values {
                    let pair = value
                        .as_array()
                        .filter(|v| v.len() == 2)
                        .ok_or_else(|| error("invalid hash algorithm configuration"))?;
                    pairs.push((
                        pair[0]
                            .as_str()
                            .ok_or_else(|| error("invalid hash metadata key"))?
                            .into(),
                        pair[1]
                            .as_str()
                            .ok_or_else(|| error("invalid hash algorithm configuration"))?
                            .into(),
                    ));
                }
            }
            _ => return Err(error("invalid hash algorithm configuration")),
        }
        if pairs.len() > MAX_HASHES
            || pairs.iter().any(|(key, algorithm): &(String, String)| {
                key.len() > 4096 || algorithm.len() > 128
            })
            || pairs.iter().map(|(k, a)| k.len() + a.len()).sum::<usize>() > 65_536
        {
            return Err(error("hash options exceed supported limits"));
        }
        for (_, algorithm) in &pairs {
            digest(algorithm)?;
        }
        let chunk = match options.get("chunk-size") {
            None => 32_768,
            Some(Value::Bool(v)) => usize::from(*v),
            Some(value) => {
                if let Some(value) = value.as_i64() {
                    if value < 0 {
                        65_536
                    } else {
                        usize::try_from(value)
                            .map_err(|_| error("hash chunk size exceeds supported limits"))?
                    }
                } else if let Some(value) = value.as_u64() {
                    usize::try_from(value)
                        .map_err(|_| error("hash chunk size exceeds supported limits"))?
                } else {
                    return Err(error("invalid hash chunk size"));
                }
            }
        };
        if chunk > MAX_CHUNK {
            return Err(error("hash chunk size exceeds supported limits"));
        }
        Ok(Self {
            hashes: pairs,
            chunk,
            filename: options.get("filename").is_some_and(truth),
        })
    }
    pub fn rebuilds_filename(&self) -> bool {
        self.filename
    }
    pub async fn run(&self, context: &mut Map<String, Value>, path: &Path) -> Result<bool> {
        let flag = Arc::new(AtomicBool::new(false));
        struct Cancel(Arc<AtomicBool>);
        impl Drop for Cancel {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let _cancel = Cancel(flag.clone());
        let options = self.clone();
        let path = path.to_owned();
        let values = tokio::task::spawn_blocking(move || options.compute(&path, &flag)).await??;
        for (key, value) in values {
            context.insert(key, Value::String(value));
        }
        Ok(self.filename)
    }
    fn compute(&self, path: &Path, flag: &AtomicBool) -> Result<Vec<(String, String)>> {
        let mut file = open_regular(path)?;
        let mut hashes = self
            .hashes
            .iter()
            .map(|(key, algorithm)| Ok((key.clone(), digest(algorithm)?)))
            .collect::<Result<Vec<_>>>()?;
        // Smaller physical reads bound cancellation latency even when the configured
        // logical chunk is large; digest results are independent of chunk boundaries.
        let mut buffer = vec![0; self.chunk.min(65_536)];
        loop {
            if flag.load(Ordering::Acquire) {
                return Err(error("hash processing cancelled"));
            }
            let count = file
                .read(&mut buffer)
                .map_err(|_| error("cannot read hash source file"))?;
            if count == 0 {
                break;
            }
            for (_, hash) in &mut hashes {
                if flag.load(Ordering::Acquire) {
                    return Err(error("hash processing cancelled"));
                }
                hash.update(&buffer[..count]);
            }
        }
        if flag.load(Ordering::Acquire) {
            return Err(error("hash processing cancelled"));
        }
        Ok(hashes
            .into_iter()
            .map(|(key, hash)| {
                let bytes = hash.finalize();
                let mut value = String::with_capacity(bytes.len() * 2);
                for byte in bytes {
                    use std::fmt::Write;
                    write!(&mut value, "{byte:02x}").unwrap();
                }
                (key, value)
            })
            .collect())
    }
}
fn open_regular(path: &Path) -> Result<File> {
    let before =
        std::fs::symlink_metadata(path).map_err(|_| error("cannot inspect hash source file"))?;
    if !before.file_type().is_file() {
        return Err(error("hash source must be a regular file"));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000);
    }
    let file = options
        .open(path)
        .map_err(|_| error("cannot open hash source file"))?;
    let after = file
        .metadata()
        .map_err(|_| error("cannot inspect hash source file"))?;
    if !after.file_type().is_file() {
        return Err(error("hash source must be a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != after.dev() || before.ino() != after.ino() {
            return Err(error("hash source changed while opening"));
        }
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn options(value: Value) -> HashOptions {
        HashOptions::parse(value.as_object().unwrap()).unwrap()
    }
    #[tokio::test]
    async fn published_digest_vectors_and_filename_truthiness() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("input");
        std::fs::write(&path, b"abc").unwrap();
        let mut context = Map::new();
        assert!(
            options(
                json!({"hashes":"md5,sha1,sha256,sha3_256,blake2s", "filename":[1],"chunk-size":1})
            )
            .run(&mut context, &path)
            .await
            .unwrap()
        );
        for (algorithm, expected) in [
            ("md5", "900150983cd24fb0d6963f7d28e17f72"),
            ("sha1", "a9993e364706816aba3e25717850c26c9cd0d89d"),
            (
                "sha256",
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                "sha3_256",
                "3a985da74fe225b2045c172d6bd390bd855f086e3e9d525b46bfe24511431532",
            ),
            (
                "blake2s",
                "508c5e8c327c14e2e1a72ba34eeb452f37458b209ed63a294d999b4c86675982",
            ),
        ] {
            assert_eq!(context[algorithm], expected);
        }
    }
    #[tokio::test]
    async fn zero_reads_empty_negative_streams_and_duplicate_keys_use_last_digest() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("input");
        std::fs::write(&path, b"abc").unwrap();
        let mut context = Map::new();
        options(json!({"chunk-size":0}))
            .run(&mut context, &path)
            .await
            .unwrap();
        assert_eq!(context["md5"], "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(context["sha1"], "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        options(json!({"chunk-size":-1,"mode":[["custom","md5"],["custom","sha1"]]}))
            .run(&mut context, &path)
            .await
            .unwrap();
        assert_eq!(
            context["custom"],
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        let empty = options(json!({"hashes":{}}));
        empty.run(&mut context, &path).await.unwrap();
        assert!(empty.hashes.is_empty());
    }
    #[tokio::test]
    async fn upstream_hash_fixture_matches_all_supported_cases() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/postprocessors/source.json"))
                .unwrap();
        for case in fixture["hash_cases"].as_array().unwrap() {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("input");
            std::fs::write(&path, case["input"].as_str().unwrap().as_bytes()).unwrap();
            let mut context = case["context"].as_object().unwrap().clone();
            let rebuild = options(case["options"].clone())
                .run(&mut context, &path)
                .await
                .unwrap();
            assert_eq!(Value::Object(context), case["result"], "{}", case["name"]);
            assert_eq!(
                rebuild,
                case["rebuild"].as_bool().unwrap(),
                "{}",
                case["name"]
            );
        }
    }
    #[test]
    fn malformed_private_options_are_bounded_and_errors_discard_values() {
        for value in [
            json!({"mode":"PRIVATE_HASH"}),
            json!({"hashes":[["key","PRIVATE_HASH"]]}),
            json!({"chunk-size":"PRIVATE_SIZE"}),
            json!({"chunk-size":16_777_217}),
            json!({"hashes":vec![["x","md5"];129]}),
        ] {
            let message = HashOptions::parse(value.as_object().unwrap())
                .unwrap_err()
                .to_string();
            assert!(!message.contains("PRIVATE"));
        }
    }
    #[test]
    fn cancelled_hashing_and_nonregular_sources_fail_without_digest_results() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("input");
        std::fs::write(&path, b"abc").unwrap();
        assert!(
            options(json!({}))
                .compute(&path, &AtomicBool::new(true))
                .is_err()
        );
        assert!(open_regular(directory.path()).is_err());
        #[cfg(unix)]
        {
            let link = directory.path().join("link");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert!(open_regular(&link).is_err());
        }
    }
}
