//! SHA256 helpers shared by the app self-updater and the browser downloader.
//!
//! Both verify a downloaded artifact against a digest published beside it, so
//! the hashing and the `sha256sum` parsing live here instead of in either
//! caller.

use std::path::Path;

/// Stream `path` through SHA256 and return the lowercase hex digest. Reads in
/// 1 MiB blocks so a multi-gigabyte browser archive never lands in memory.
pub fn sha256_file(path: &Path) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
  use sha2::{Digest, Sha256};
  use std::io::Read;
  let mut file = std::fs::File::open(path)?;
  let mut hasher = Sha256::new();
  let mut buf = vec![0u8; 1024 * 1024];
  loop {
    let n = file.read(&mut buf)?;
    if n == 0 {
      break;
    }
    hasher.update(&buf[..n]);
  }
  let digest = hasher.finalize();
  let mut hex = String::with_capacity(digest.len() * 2);
  for byte in digest {
    use std::fmt::Write;
    let _ = write!(hex, "{byte:02x}");
  }
  Ok(hex)
}

/// Extract the hex digest for `filename` from standard `sha256sum` output
/// (`<hex>  <name>`, optionally with the `*` binary-mode marker).
pub fn find_checksum_for_file(checksums_text: &str, filename: &str) -> Option<String> {
  checksums_text.lines().find_map(|line| {
    let (hash, rest) = line.split_once(char::is_whitespace)?;
    let name = rest.trim_start().trim_start_matches('*');
    if name == filename && is_sha256_hex(hash) {
      Some(hash.to_ascii_lowercase())
    } else {
      None
    }
  })
}

fn is_sha256_hex(value: &str) -> bool {
  value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
  use super::*;

  const HELLO_WORLD_SHA256: &str =
    "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";

  #[test]
  fn test_find_checksum_for_file() {
    let sums = "\
0e5a4601745092b7d1c93c1e7e1c30d923be3d1e916b661bd53d1c0c9c7f0a11  Donut_0.29.0_aarch64.dmg
ABCDEF01745092B7D1C93C1E7E1C30D923BE3D1E916B661BD53D1C0C9C7F0A22 *Donut_0.29.0_x64.dmg
not-a-hash  Donut_0.29.0_amd64.deb
";

    // Plain entry.
    assert_eq!(
      find_checksum_for_file(sums, "Donut_0.29.0_aarch64.dmg").as_deref(),
      Some("0e5a4601745092b7d1c93c1e7e1c30d923be3d1e916b661bd53d1c0c9c7f0a11")
    );
    // Binary-mode marker is stripped; hash is normalized to lowercase.
    assert_eq!(
      find_checksum_for_file(sums, "Donut_0.29.0_x64.dmg").as_deref(),
      Some("abcdef01745092b7d1c93c1e7e1c30d923be3d1e916b661bd53d1c0c9c7f0a22")
    );
    // Entries with malformed hashes are rejected rather than trusted.
    assert_eq!(find_checksum_for_file(sums, "Donut_0.29.0_amd64.deb"), None);
    // Missing file.
    assert_eq!(find_checksum_for_file(sums, "Donut_0.29.0_arm64.deb"), None);
  }

  #[test]
  fn test_sha256_file_matches_known_digest() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let path = temp_dir.path().join("data.bin");
    std::fs::write(&path, b"hello world").unwrap();
    assert_eq!(sha256_file(&path).unwrap(), HELLO_WORLD_SHA256);
  }
}
