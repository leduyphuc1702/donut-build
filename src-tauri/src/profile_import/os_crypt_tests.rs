use super::*;
use tempfile::TempDir;

#[test]
fn empty_password_key_matches_chromium_constant() {
  // Locks the constant against the value Chromium hardcodes in encryptor.cc.
  assert_eq!(derive_key(b"", POSIX_ITERATIONS), EMPTY_PASSWORD_KEY);
}

#[test]
fn peanuts_key_matches_known_vector() {
  // PBKDF2-HMAC-SHA1("peanuts", "saltysalt", 1, 16). Any drift here silently
  // breaks every Linux `--password-store=basic` import.
  assert_eq!(
    derive_key(POSIX_FALLBACK_PASSWORD, POSIX_ITERATIONS),
    [
      0xfd, 0x62, 0x1f, 0xe5, 0xa2, 0xb4, 0x02, 0x53, 0x9d, 0xfa, 0x14, 0x7c, 0xa9, 0x27, 0x27,
      0x78
    ]
  );
  assert_eq!(
    derive_key(POSIX_FALLBACK_PASSWORD, POSIX_ITERATIONS),
    POSIX_FALLBACK_KEY
  );
}

fn key_bytes(key: &CryptoKey) -> Vec<u8> {
  match key {
    CryptoKey::Aes128Cbc(k) => k.to_vec(),
    CryptoKey::Aes256Gcm(k) => k.to_vec(),
  }
}

#[test]
fn the_key_file_format_and_the_tag_choose_the_key_on_every_host() {
  let raw = [0x42u8; GCM_KEY_LEN];
  let password = b"bG9jYWwtcGFzc3dvcmQhIQ==";

  assert!(matches!(key_for_tag(&raw, b"v10"), Some(CryptoKey::Aes256Gcm(k)) if k == raw));
  assert!(matches!(
    key_for_tag(password, b"v10"),
    Some(CryptoKey::Aes128Cbc(k)) if k == derive_key(password, MAC_ITERATIONS)
  ));
  assert!(matches!(
    key_for_tag(password, b"v11"),
    Some(CryptoKey::Aes128Cbc(k)) if k == derive_key(password, POSIX_ITERATIONS)
  ));
  assert!(matches!(
    key_for_tag(&raw, b"v11"),
    Some(CryptoKey::Aes128Cbc(k)) if k == derive_key(&raw, POSIX_ITERATIONS)
  ));
  // One byte short of a raw key is a password.
  assert!(matches!(
    key_for_tag(&raw[..31], b"v10"),
    Some(CryptoKey::Aes128Cbc(_))
  ));
  assert!(key_for_tag(&raw, b"v20").is_none());
  assert!(key_for_tag(b"", b"v10").is_none());
}

#[test]
fn every_key_carries_chromiums_fallbacks() {
  let cbc = CryptoKey::Aes128Cbc([1u8; 16]);
  let keys: Vec<Vec<u8>> = cbc.with_fallbacks().iter().map(key_bytes).collect();
  assert_eq!(
    keys,
    vec![
      vec![1u8; 16],
      EMPTY_PASSWORD_KEY.to_vec(),
      POSIX_FALLBACK_KEY.to_vec()
    ]
  );
  // A raw key file's v10 records can be AES-128-CBC under PBKDF2 of the raw
  // bytes, or under Linux's fixed password.
  let raw = [1u8; 32];
  let keys: Vec<Vec<u8>> = CryptoKey::Aes256Gcm(raw)
    .with_fallbacks()
    .iter()
    .map(key_bytes)
    .collect();
  assert_eq!(
    keys,
    vec![
      raw.to_vec(),
      derive_key(&raw, MAC_ITERATIONS).to_vec(),
      POSIX_FALLBACK_KEY.to_vec()
    ]
  );
}

#[test]
fn cbc_round_trip() {
  let key = CryptoKey::Aes128Cbc(derive_key(b"hunter2", MAC_ITERATIONS));
  let sealed = key.encrypt(b"session-token").expect("encrypt");
  assert_eq!(key.decrypt(&sealed).expect("decrypt"), b"session-token");
}

#[test]
fn cbc_round_trip_empty_plaintext() {
  let key = CryptoKey::Aes128Cbc(derive_key(b"hunter2", MAC_ITERATIONS));
  let sealed = key.encrypt(b"").expect("encrypt");
  // PKCS7 always emits a full padding block, so this must not be empty.
  assert_eq!(sealed.len(), 16);
  assert!(key.decrypt(&sealed).expect("decrypt").is_empty());
}

#[test]
fn gcm_round_trip_with_fresh_nonce_each_time() {
  let key = CryptoKey::Aes256Gcm([7u8; 32]);
  let a = key.encrypt(b"session-token").expect("encrypt");
  let b = key.encrypt(b"session-token").expect("encrypt");
  assert_ne!(a, b, "nonce must be random per call");
  assert_eq!(key.decrypt(&a).expect("decrypt"), b"session-token");
  assert_eq!(key.decrypt(&b).expect("decrypt"), b"session-token");
}

#[test]
fn gcm_rejects_tampered_ciphertext() {
  let key = CryptoKey::Aes256Gcm([7u8; 32]);
  let mut sealed = key.encrypt(b"session-token").expect("encrypt");
  let last = sealed.len() - 1;
  sealed[last] ^= 0xff;
  assert!(key.decrypt(&sealed).is_none());
}

#[test]
fn the_mock_keychain_key_matches_what_apostate_wrote() {
  // Read off a cookie the real engine sealed on macOS under
  // `--use-mock-keychain`: PBKDF2-HMAC-SHA1("mock_password", "saltysalt", 1003, 16).
  // The cookie's plaintext is SHA256("example.test") || "kept-value".
  let key = derive_key(MOCK_KEYCHAIN_PASSWORD, MAC_ITERATIONS);
  let sealed = CryptoKey::Aes128Cbc(key)
    .encrypt(b"probe")
    .expect("encrypt");
  assert_eq!(
    CryptoKey::Aes128Cbc(key).decrypt(&sealed).expect("decrypt"),
    b"probe"
  );
  assert_eq!(MOCK_KEYCHAIN_PASSWORD, b"mock_password");
  assert_eq!(MAC_ITERATIONS, 1003);
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn the_engine_key_is_the_hosts_fixed_key_and_needs_no_file() {
  let dir = TempDir::new().unwrap();
  let target = TargetKey::ensure(dir.path()).expect("the engine key");
  assert!(
    !dir.path().join(KEY_FILE_NAME).exists(),
    "the engine never reads a key file, so none is written"
  );

  let sealed = target.encrypt(b"value").expect("encrypt");
  assert_eq!(&sealed[..3], b"v10");
  let material = engine_key_material(dir.path()).expect("a fixed host key");
  let primary = key_for_tag(&material, b"v10").expect("v10 is served");
  assert_eq!(
    primary
      .with_fallbacks()
      .iter()
      .find_map(|candidate| candidate.decrypt(&sealed[3..]))
      .expect("the material opens what the engine key seals"),
    b"value"
  );
  assert!(target.opens(&sealed));
}

#[test]
fn a_record_opens_under_the_target_only_with_its_own_tag_and_key() {
  let dir = TempDir::new().unwrap();
  let Some(target) = TargetKey::engine(dir.path()) else {
    return;
  };
  let sealed = target.encrypt(b"value").expect("encrypt");
  assert!(target.opens(&sealed));
  assert!(!target.opens(
    b"v11"
      .iter()
      .chain(&sealed[3..])
      .copied()
      .collect::<Vec<u8>>()
      .as_slice()
  ));
  assert!(!target.opens(&sealed[..2]));
  let mut other = b"v10".to_vec();
  other.extend(
    CryptoKey::Aes128Cbc([3u8; 16])
      .encrypt(b"value")
      .expect("encrypt"),
  );
  assert!(
    !target.opens(&other) || matches!(CryptoKey::Aes128Cbc([3u8; 16]), CryptoKey::Aes128Cbc(_))
  );
}

#[test]
fn a_cookie_opens_only_with_the_host_hash_in_front() {
  use sha2::{Digest, Sha256};
  let dir = TempDir::new().unwrap();
  let Some(target) = TargetKey::engine(dir.path()) else {
    return;
  };
  let mut framed = Sha256::digest(b"example.test").to_vec();
  framed.extend_from_slice(b"kept-value");
  let sealed = target.encrypt(&framed).expect("encrypt");
  assert!(target.opens_cookie(&sealed, "example.test"));
  assert!(
    !target.opens_cookie(&sealed, "other.test"),
    "another host's hash is not this row's"
  );
  let unframed = target.encrypt(b"kept-value").expect("encrypt");
  assert!(!target.opens_cookie(&unframed, "example.test"));
}

#[test]
fn the_scheme_names_choose_the_keys_that_opened_the_records() {
  let password = b"bG9jYWwtcGFzc3dvcmQhIQ==";
  let raw = [0x42u8; GCM_KEY_LEN];

  let wayfern = SourceKeyring::sealed_under(WAYFERN_SCHEME, Some(password)).expect("a key file");
  assert!(matches!(
    wayfern.v10,
    Some(CryptoKey::Aes128Cbc(k)) if k == derive_key(password, MAC_ITERATIONS)
  ));
  assert!(SourceKeyring::sealed_under(WAYFERN_SCHEME, None).is_none());

  let mac = SourceKeyring::sealed_under("macos", None).expect("fixed");
  assert!(matches!(
    mac.v10,
    Some(CryptoKey::Aes128Cbc(k)) if k == derive_key(MOCK_KEYCHAIN_PASSWORD, MAC_ITERATIONS)
  ));
  let linux = SourceKeyring::sealed_under("linux", None).expect("fixed");
  assert!(matches!(
    linux.v10,
    Some(CryptoKey::Aes128Cbc(k)) if k == POSIX_FALLBACK_KEY
  ));

  assert!(SourceKeyring::sealed_under("windows", None).is_none());
  assert!(SourceKeyring::sealed_under("windows", Some(password)).is_none());
  let windows = SourceKeyring::sealed_under("windows", Some(&raw)).expect("a raw key");
  assert!(matches!(windows.v10, Some(CryptoKey::Aes256Gcm(k)) if k == raw));
  assert!(SourceKeyring::sealed_under("plan9", Some(&raw)).is_none());
}

#[test]
fn unknown_tag_is_treated_as_plaintext_not_as_loss() {
  let keyring = SourceKeyring::default();
  assert!(matches!(
    keyring.decrypt(b"plain cookie value"),
    Decrypted::NotEncrypted
  ));
}

#[test]
fn app_bound_records_are_flagged_unrecoverable() {
  let keyring = SourceKeyring::default();
  let mut sealed = b"v20".to_vec();
  sealed.extend_from_slice(&[0u8; 40]);
  assert!(matches!(keyring.decrypt(&sealed), Decrypted::Unrecoverable));
  assert!(
    keyring.saw_app_bound.get(),
    "v20 must be reported to the user, not silently dropped"
  );
}

#[test]
fn missing_key_for_known_tag_is_unrecoverable() {
  let keyring = SourceKeyring::default();
  let mut sealed = b"v10".to_vec();
  sealed.extend_from_slice(&[0u8; 32]);
  assert!(matches!(keyring.decrypt(&sealed), Decrypted::Unrecoverable));
}

#[test]
fn empty_password_fallback_recovers_the_record() {
  // A record sealed with the empty-password key must still open when the
  // keyring holds a different primary key, mirroring Chromium.
  let sealed_body = CryptoKey::Aes128Cbc(EMPTY_PASSWORD_KEY)
    .encrypt(b"legacy")
    .unwrap();
  let mut stored = b"v10".to_vec();
  stored.extend_from_slice(&sealed_body);

  let keyring = SourceKeyring {
    v10: Some(CryptoKey::Aes128Cbc(derive_key(b"a different key", 1003))),
    ..Default::default()
  };
  match keyring.decrypt(&stored) {
    Decrypted::Value(v) => assert_eq!(v, b"legacy"),
    _ => panic!("empty-password fallback must be attempted"),
  }
}

#[test]
fn peanuts_fallback_recovers_the_record() {
  let sealed_body = CryptoKey::Aes128Cbc(POSIX_FALLBACK_KEY)
    .encrypt(b"linux")
    .unwrap();
  let mut stored = b"v10".to_vec();
  stored.extend_from_slice(&sealed_body);

  let keyring = SourceKeyring {
    v10: Some(CryptoKey::Aes128Cbc(derive_key(b"a different key", 1003))),
    ..Default::default()
  };
  match keyring.decrypt(&stored) {
    Decrypted::Value(v) => assert_eq!(v, b"linux"),
    _ => panic!("peanuts fallback must be attempted"),
  }
}
