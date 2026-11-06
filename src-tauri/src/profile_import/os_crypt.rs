//! Key material for profile import, and the key Apostate seals secrets with.
//!
//! Apostate is stock Chromium here: it never reads `<user-data-dir>/os_crypt_key`
//! (that file was Wayfern's). The key it seals cookies, logins and cards with is
//! whatever the host's provider gives it, and Donut starts it so that key is
//! fixed and known ([`engine_key_material`]):
//!
//! | Host    | Provider                                   | Records                                 |
//! |---------|--------------------------------------------|-----------------------------------------|
//! | macOS   | `--use-mock-keychain`, password `mock_password` | `v10`, AES-128-CBC, PBKDF2 1003 rounds |
//! | Linux   | `--password-store=basic`, password `peanuts`    | `v10`, AES-128-CBC, PBKDF2 1 round     |
//! | Windows | the key DPAPI-wrapped in `Local State`          | `v10`, AES-256-GCM                     |
//!
//! Anything Donut writes into a profile Apostate will open (an import, a profile
//! Wayfern sealed with its key file) is sealed with that key ([`TargetKey`]).
//!
//! The rest of this page is the key file format, which Wayfern used for every
//! profile and which still carries the *source* side of a re-seal.
//!
//! A Wayfern profile kept its os_crypt key material in
//! `<user-data-dir>/os_crypt_key` rather than the OS keyring, so a profile
//! directory was self-contained and portable.
//!
//! That portability is exactly why an imported Chrome profile carries nothing:
//! its secrets are sealed with a key held in the macOS Keychain / Windows DPAPI
//! / the Freedesktop secret service, which the profile directory does not
//! contain. Import has to open the source's lock and re-seal everything with
//! the profile's own key.
//!
//! The key file holds one of Chromium's two key formats: the raw 32-byte
//! AES-256-GCM key Chromium uses on Windows, or the password Chromium feeds to
//! PBKDF2 on macOS and Linux. The format of the file and the tag of each record
//! select the cipher, the same on every host ([`key_for_tag`]):
//!
//! | `os_crypt_key`           | Tag   | Cipher       | Key                                       |
//! |--------------------------|-------|--------------|-------------------------------------------|
//! | 32 raw bytes             | `v10` | AES-256-GCM  | the bytes                                 |
//! | any other bytes          | `v10` | AES-128-CBC  | PBKDF2-HMAC-SHA1(contents, saltysalt, 1003) |
//! | any                      | `v11` | AES-128-CBC  | PBKDF2-HMAC-SHA1(contents, saltysalt, 1)  |
//!
//! A new key file is minted in the host's format and records are written under
//! the host's tag (`v10` on macOS and Windows, `v11` on Linux), so a profile
//! created here reads exactly like one Chromium wrote on this OS. Linux writes
//! `v11`, not `v10`: Chromium's `PosixKeyProvider` owns `v10` there.

use aes::cipher::{block_padding::Pkcs7, BlockModeDecrypt, BlockModeEncrypt, KeyIvInit};
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use rand::RngExt;
use ring::pbkdf2;
use std::num::NonZeroU32;
use std::path::Path;

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;
type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;

/// Chromium's fixed PBKDF2 salt for every CBC-based os_crypt provider.
pub const SALT: &[u8] = b"saltysalt";
/// Chromium's fixed CBC IV: sixteen spaces.
pub const CBC_IV: [u8; 16] = [b' '; 16];
/// AES-256-GCM nonce length, prepended to the ciphertext by `Encryptor::Key::Encrypt`.
const GCM_NONCE_LEN: usize = 12;
/// Length of a raw AES-256-GCM key file, the format Chromium uses on Windows.
pub const GCM_KEY_LEN: usize = 32;
/// The `os_crypt_key` name, at the root of the user-data dir.
pub const KEY_FILE_NAME: &str = "os_crypt_key";

/// `PBKDF2-HMAC-SHA1(password = "", salt = "saltysalt", iterations = 1)`.
///
/// Chromium retries every failed AES-128-CBC decrypt with this key
/// (`encryptor.cc`, crbug.com/40055416) because profiles created while the
/// keyring was unavailable were sealed with an empty password. Import has to do
/// the same or those records look corrupt.
pub const EMPTY_PASSWORD_KEY: [u8; 16] = [
  0xd0, 0xd0, 0xec, 0x9c, 0x7d, 0x77, 0xd4, 0x3a, 0xc5, 0x41, 0x87, 0xfa, 0x48, 0x18, 0xd1, 0x7f,
];

/// The password Chromium's `PosixKeyProvider` uses when no secret service is
/// available (`--password-store=basic`). Records sealed with it carry `v10`.
// Read on Linux and by the known-answer tests; unreferenced on other hosts.
#[allow(dead_code)]
pub const POSIX_FALLBACK_PASSWORD: &[u8] = b"peanuts";

/// What a profile's stores are sealed under while its key file is the key
/// (Wayfern), as opposed to an OS's engine key.
pub const WAYFERN_SCHEME: &str = "wayfern";

/// The password Chromium's mock keychain hands out (`--use-mock-keychain`),
/// which Apostate is started with on macOS.
// Read on macOS and by the known-answer tests; unreferenced on other hosts.
#[allow(dead_code)]
pub const MOCK_KEYCHAIN_PASSWORD: &[u8] = b"mock_password";

/// `PBKDF2-HMAC-SHA1(password = "peanuts", salt = "saltysalt", iterations = 1)`.
///
/// Linux Chromium seals `v10` records under this fixed key when it has no
/// other, and such records keep it when the profile moves to another OS, so
/// every AES-128-CBC decrypt falls back to it after [`EMPTY_PASSWORD_KEY`].
pub const POSIX_FALLBACK_KEY: [u8; 16] = [
  0xfd, 0x62, 0x1f, 0xe5, 0xa2, 0xb4, 0x02, 0x53, 0x9d, 0xfa, 0x14, 0x7c, 0xa9, 0x27, 0x27, 0x78,
];

/// PBKDF2 iteration counts: macOS Chromium derives its `v10` key with 1003,
/// Linux Chromium derives its keys with 1.
pub const MAC_ITERATIONS: u32 = 1003;
pub const POSIX_ITERATIONS: u32 = 1;

/// Derive a 16-byte AES-128 key the way every CBC os_crypt provider does.
///
/// `password` is the raw bytes, never trimmed: Chromium passes the exact
/// `ReadFileToString` result to the KDF, so normalising here would silently
/// produce a different key and every decrypt would fail.
pub fn derive_key(password: &[u8], iterations: u32) -> [u8; 16] {
  let mut key = [0u8; 16];
  // ring rather than the `pbkdf2` crate: sha1 0.11 (digest 0.11) and
  // pbkdf2 0.12 (digest 0.10) cannot coexist. ring is self-contained.
  pbkdf2::derive(
    pbkdf2::PBKDF2_HMAC_SHA1,
    NonZeroU32::new(iterations).expect("iterations must be non-zero"),
    SALT,
    password,
    &mut key,
  );
  key
}

/// The key for records tagged `tag` in a profile whose `os_crypt_key` holds
/// `contents`, or `None` for an empty file or a tag no key file serves. The
/// file's format decides, never the host, so a profile written on any OS reads
/// the same everywhere:
///
/// - 32 raw bytes: `v10` is AES-256-GCM keyed by the bytes.
/// - any other contents: `v10` is AES-128-CBC keyed by
///   PBKDF2-HMAC-SHA1(contents, "saltysalt", 1003).
/// - any contents: `v11` is AES-128-CBC keyed by
///   PBKDF2-HMAC-SHA1(contents, "saltysalt", 1).
pub fn key_for_tag(contents: &[u8], tag: &[u8]) -> Option<CryptoKey> {
  if contents.is_empty() {
    return None;
  }
  match tag {
    b"v10" => Some(match <[u8; GCM_KEY_LEN]>::try_from(contents) {
      Ok(raw) => CryptoKey::Aes256Gcm(raw),
      Err(_) => CryptoKey::Aes128Cbc(derive_key(contents, MAC_ITERATIONS)),
    }),
    b"v11" => Some(CryptoKey::Aes128Cbc(derive_key(contents, POSIX_ITERATIONS))),
    _ => None,
  }
}

/// The key Apostate seals records with on this host, as `os_crypt_key` file
/// contents ([`key_for_tag`] turns it into the cipher), or `None` where the OS
/// holds it and the profile has none yet: Windows before the engine first ran
/// on the profile. Linux's `v10` records are keyed by PBKDF2 with one round,
/// which `key_for_tag` reaches through [`CryptoKey::with_fallbacks`].
pub fn engine_key_material(user_data_dir: &Path) -> Option<Vec<u8>> {
  #[cfg(target_os = "macos")]
  {
    let _ = user_data_dir;
    Some(MOCK_KEYCHAIN_PASSWORD.to_vec())
  }
  #[cfg(target_os = "linux")]
  {
    let _ = user_data_dir;
    Some(POSIX_FALLBACK_PASSWORD.to_vec())
  }
  #[cfg(target_os = "windows")]
  {
    super::keyring::windows_local_state_key(user_data_dir)
      .ok()
      .flatten()
      .map(|key| key.to_vec())
  }
  #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
  {
    let _ = user_data_dir;
    None
  }
}

/// One os_crypt cipher, keyed. Which variant applies is decided by the tag the
/// record carries and the format of the key file, never by the host platform.
#[derive(Clone)]
pub enum CryptoKey {
  Aes128Cbc([u8; 16]),
  Aes256Gcm([u8; 32]),
}

impl CryptoKey {
  /// This key, then, for AES-128-CBC, the fixed keys Chromium retries a CBC
  /// record with when it does not open: the empty password
  /// ([`EMPTY_PASSWORD_KEY`]) and Linux's fixed password
  /// ([`POSIX_FALLBACK_KEY`]). A raw key file's `v10` records can also be
  /// AES-128-CBC, keyed by PBKDF2(the 32 bytes, "saltysalt", 1003) or by
  /// Linux's fixed password, so an AES-256-GCM key falls back to those two.
  pub fn with_fallbacks(&self) -> Vec<CryptoKey> {
    match self {
      Self::Aes128Cbc(_) => vec![
        self.clone(),
        Self::Aes128Cbc(EMPTY_PASSWORD_KEY),
        Self::Aes128Cbc(POSIX_FALLBACK_KEY),
      ],
      Self::Aes256Gcm(raw) => vec![
        self.clone(),
        Self::Aes128Cbc(derive_key(raw, MAC_ITERATIONS)),
        Self::Aes128Cbc(POSIX_FALLBACK_KEY),
      ],
    }
  }

  /// Decrypt a *tagless* ciphertext (the caller has already stripped the
  /// 3-byte version prefix).
  pub fn decrypt(&self, ciphertext: &[u8]) -> Option<Vec<u8>> {
    match self {
      Self::Aes128Cbc(key) => {
        if ciphertext.is_empty() {
          return Some(Vec::new());
        }
        let mut buf = ciphertext.to_vec();
        Aes128CbcDec::new(key.into(), &CBC_IV.into())
          .decrypt_padded::<Pkcs7>(&mut buf)
          .ok()
          .map(<[u8]>::to_vec)
      }
      Self::Aes256Gcm(key) => {
        if ciphertext.len() < GCM_NONCE_LEN {
          return None;
        }
        let (nonce, body) = ciphertext.split_at(GCM_NONCE_LEN);
        let nonce: [u8; GCM_NONCE_LEN] = nonce.try_into().ok()?;
        Aes256Gcm::new(&Key::<Aes256Gcm>::from(*key))
          .decrypt(
            &Nonce::from(nonce),
            Payload {
              msg: body,
              aad: &[],
            },
          )
          .ok()
      }
    }
  }

  /// Encrypt to a *tagless* ciphertext. The caller prepends the tag.
  pub fn encrypt(&self, plaintext: &[u8]) -> Option<Vec<u8>> {
    match self {
      Self::Aes128Cbc(key) => {
        let mut buf = vec![0u8; plaintext.len() + 16];
        buf[..plaintext.len()].copy_from_slice(plaintext);
        Aes128CbcEnc::new(key.into(), &CBC_IV.into())
          .encrypt_padded::<Pkcs7>(&mut buf, plaintext.len())
          .ok()
          .map(<[u8]>::to_vec)
      }
      Self::Aes256Gcm(key) => {
        let nonce: [u8; GCM_NONCE_LEN] = rand::rng().random();
        let sealed = Aes256Gcm::new(&Key::<Aes256Gcm>::from(*key))
          .encrypt(
            &Nonce::from(nonce),
            Payload {
              msg: plaintext,
              aad: &[],
            },
          )
          .ok()?;
        // The nonce goes at the front, matching `Encryptor::Key::Encrypt`.
        let mut out = Vec::with_capacity(GCM_NONCE_LEN + sealed.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&sealed);
        Some(out)
      }
    }
  }
}

/// The key records written into a profile are sealed with, so the engine that
/// opens the profile can read them.
pub struct TargetKey {
  key: CryptoKey,
  tag: &'static [u8; 3],
}

impl TargetKey {
  /// Apostate's own key on this host ([`engine_key_material`]), or `None` where
  /// the OS holds it and the profile has none yet.
  pub fn engine(user_data_dir: &Path) -> Option<Self> {
    #[cfg(target_os = "macos")]
    {
      let _ = user_data_dir;
      Some(Self {
        key: CryptoKey::Aes128Cbc(derive_key(MOCK_KEYCHAIN_PASSWORD, MAC_ITERATIONS)),
        tag: b"v10",
      })
    }
    #[cfg(target_os = "linux")]
    {
      let _ = user_data_dir;
      Some(Self {
        key: CryptoKey::Aes128Cbc(POSIX_FALLBACK_KEY),
        tag: b"v10",
      })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
      let material = engine_key_material(user_data_dir)?;
      Some(Self {
        key: key_for_tag(&material, b"v10")?,
        tag: b"v10",
      })
    }
  }

  /// Whether `stored` already opens under this key: a record sealed for the
  /// engine, which a re-seal must leave alone.
  pub fn opens(&self, stored: &[u8]) -> bool {
    match stored.split_at_checked(3) {
      Some((tag, body)) => tag == self.tag && self.key.decrypt(body).is_some(),
      None => false,
    }
  }

  /// [`opens`](Self::opens) for a cookie row, which also carries the
  /// `SHA256(host_key)` prefix the plaintext must start with.
  pub fn opens_cookie(&self, stored: &[u8], host_key: &str) -> bool {
    use sha2::{Digest, Sha256};
    let Some((tag, body)) = stored.split_at_checked(3) else {
      return false;
    };
    if tag != self.tag {
      return false;
    }
    let Some(plain) = self.key.decrypt(body) else {
      return false;
    };
    let expected: [u8; 32] = Sha256::digest(host_key.as_bytes()).into();
    plain.len() >= 32 && plain[..32] == expected
  }

  /// The engine's key for a profile that is about to be written into: the
  /// host's own key where it is fixed, and on Windows, where the OS holds it,
  /// the key already in `Local State` or a new one put there.
  ///
  /// Writing eagerly (rather than letting the first launch mint it) is
  /// deliberate: a key the browser cannot persist would live only in memory,
  /// die with the process, and orphan everything written with it. Failing here
  /// instead turns that silent data loss into a visible import error.
  pub fn ensure(user_data_dir: &Path) -> Result<Self, String> {
    Self::adopting(user_data_dir, &[])
  }

  /// [`ensure`](Self::ensure) for a profile Wayfern sealed with the key file
  /// `legacy`. Where the OS holds the engine's key and the profile has none,
  /// a raw 32-byte legacy key becomes it, so everything Wayfern sealed already
  /// opens; any other legacy key is replaced by a new one.
  pub fn adopting(user_data_dir: &Path, legacy: &[u8]) -> Result<Self, String> {
    if let Some(engine) = Self::engine(user_data_dir) {
      return Ok(engine);
    }
    Self::seed_local_state_key(user_data_dir, legacy)
  }

  #[cfg(target_os = "windows")]
  fn seed_local_state_key(user_data_dir: &Path, preferred: &[u8]) -> Result<Self, String> {
    std::fs::create_dir_all(user_data_dir)
      .map_err(|e| format!("Failed to create profile directory: {e}"))?;
    let key: [u8; 32] = match <[u8; 32]>::try_from(preferred) {
      Ok(raw) => raw,
      Err(_) => rand::rng().random(),
    };
    super::keyring::write_local_state_key(user_data_dir, &key)?;
    Ok(Self {
      key: CryptoKey::Aes256Gcm(key),
      tag: b"v10",
    })
  }

  #[cfg(not(target_os = "windows"))]
  fn seed_local_state_key(_user_data_dir: &Path, _preferred: &[u8]) -> Result<Self, String> {
    Err("this host has no engine key to seed".to_string())
  }

  /// Seal a value the way the engine expects to find it: `tag || ciphertext`.
  pub fn encrypt(&self, plaintext: &[u8]) -> Option<Vec<u8>> {
    let body = self.key.encrypt(plaintext)?;
    let mut out = Vec::with_capacity(3 + body.len());
    out.extend_from_slice(self.tag);
    out.extend_from_slice(&body);
    Some(out)
  }
}

/// What a decrypt attempt produced.
pub enum Decrypted {
  /// Recovered plaintext.
  Value(Vec<u8>),
  /// Already plaintext — no recognised version tag.
  NotEncrypted,
  /// Correctly identified but not openable: no key for the tag (Windows
  /// App-Bound `v20`), or every candidate key failed.
  Unrecoverable,
}

/// The source browser's keys, indexed by the tag the records carry.
///
/// Indexing by tag rather than by platform is not pedantry: a single Linux
/// profile can legitimately hold both `v10` (peanuts) and `v11` (keyring)
/// records, because the available secret service changes between sessions.
#[derive(Default)]
pub struct SourceKeyring {
  pub v10: Option<CryptoKey>,
  pub v11: Option<CryptoKey>,
  /// Seen at least one `v20` (Windows App-Bound) record, which no third party
  /// can open. Tracked so the import report can say so explicitly.
  pub saw_app_bound: std::cell::Cell<bool>,
}

impl SourceKeyring {
  pub fn is_empty(&self) -> bool {
    self.v10.is_none() && self.v11.is_none()
  }

  /// The keys a Wayfern `os_crypt_key` file opens, for each tag it serves.
  pub fn from_key_file(contents: &[u8]) -> Self {
    Self {
      v10: key_for_tag(contents, b"v10"),
      v11: key_for_tag(contents, b"v11"),
      ..Default::default()
    }
  }

  /// The keys that open what `scheme` sealed: `WAYFERN_SCHEME` (the profile's
  /// own key file), or the name of the OS whose engine sealed it
  /// (`std::env::consts::OS`). `key_file` is the profile's `os_crypt_key`,
  /// which a Windows engine's records also need: its key is the one Donut put
  /// in `Local State`. `None` when the keys cannot be known here.
  pub fn sealed_under(scheme: &str, key_file: Option<&[u8]>) -> Option<Self> {
    let v10 = match scheme {
      WAYFERN_SCHEME => return key_file.map(Self::from_key_file),
      "macos" => CryptoKey::Aes128Cbc(derive_key(MOCK_KEYCHAIN_PASSWORD, MAC_ITERATIONS)),
      "linux" => CryptoKey::Aes128Cbc(POSIX_FALLBACK_KEY),
      "windows" => CryptoKey::Aes256Gcm(<[u8; GCM_KEY_LEN]>::try_from(key_file?).ok()?),
      _ => return None,
    };
    Some(Self {
      v10: Some(v10),
      ..Default::default()
    })
  }

  /// Open one stored value, dispatching on its version tag exactly as
  /// `Encryptor::DecryptData` does.
  pub fn decrypt(&self, stored: &[u8]) -> Decrypted {
    if stored.len() < 3 {
      return if stored.is_empty() {
        Decrypted::Value(Vec::new())
      } else {
        Decrypted::NotEncrypted
      };
    }

    let (tag, body) = stored.split_at(3);
    let key = match tag {
      b"v10" => self.v10.as_ref(),
      b"v11" => self.v11.as_ref(),
      b"v20" => {
        // App-Bound Encryption. The key is wrapped by the SYSTEM-level Chrome
        // Elevation Service, which validates the calling binary. There is no
        // legitimate way for us to unwrap it.
        self.saw_app_bound.set(true);
        return Decrypted::Unrecoverable;
      }
      _ => return Decrypted::NotEncrypted,
    };

    let Some(key) = key else {
      return Decrypted::Unrecoverable;
    };

    // Chromium's own fallbacks for CBC records sealed with a fixed key.
    match key
      .with_fallbacks()
      .iter()
      .find_map(|candidate| candidate.decrypt(body))
    {
      Some(plaintext) => Decrypted::Value(plaintext),
      None => Decrypted::Unrecoverable,
    }
  }
}

#[cfg(test)]
#[path = "os_crypt_tests.rs"]
mod tests;
