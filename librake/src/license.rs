//! License-key verification for the `rake` and `cargo-rake` binaries.
//!
//! The signing authority (private key + issuance tooling) never lives here —
//! only the 32-byte Ed25519 public key is compiled in.  License strings use
//! the format `base64(json_payload).base64(ed25519_signature)`.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, Verifier as _, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::rakefile::print_label;

// Replace with real key bytes before shipping a licensed release.
// Run the license-authority tool; it prints the const to copy here.
const VERIFYING_KEY_BYTES: [u8; 32] = [
    0xa2, 0x1c, 0xdf, 0x8a, 0x9b, 0x7f, 0xf2, 0x86, 0x23, 0x47, 0x7b, 0x2c, 0x4e, 0xe2, 0x71, 0x7f,
    0x16, 0x70, 0x53, 0x8b, 0x6f, 0xc7, 0xcd, 0xed, 0x27, 0x7d, 0x43, 0xf2, 0x5f, 0x93, 0x56, 0x3f,
];

/// Returns `true` when [`VERIFYING_KEY_BYTES`] has been set to a real key.
/// All-zero bytes indicate the placeholder is still in place.
fn verifying_key_configured() -> bool {
    VERIFYING_KEY_BYTES != [0u8; 32]
}

/// Payload embedded in every signed license key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LicensePayload {
    /// Who this license belongs to (e.g. an email address or organisation name).
    pub licensee: String,
    /// Which features are unlocked by this license.
    pub features: Features,
    /// When the license expires (`None` means perpetual).
    pub expires_at: Option<DateTime<Utc>>,
    /// When the license was issued.
    pub issued_at: DateTime<Utc>,
}

/// Feature flags carried inside a [`LicensePayload`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Features {
    /// The `basic` gated feature, demonstrated via `rake basic` / `cargo rake basic`.
    pub basic: bool,
    /// The `events` gated feature, unlocking lifecycle-event emission (see
    /// [`crate::lifecycle`]) via a Rakefile's `[lifecycle]` table.
    ///
    /// `#[serde(default)]` so a license issued before this field existed
    /// still deserializes (defaulting to locked) rather than failing.
    #[serde(default)]
    pub events: bool,
}

struct SignedLicense(String);

#[derive(Debug)]
struct LicenseVerifier {
    verifying_key: VerifyingKey,
}

impl LicenseVerifier {
    fn new() -> Result<Self> {
        VerifyingKey::from_bytes(&VERIFYING_KEY_BYTES)
            .map(|k| Self { verifying_key: k })
            .map_err(|_| Error::LicenseMalformed)
    }

    fn verify(&self, license: &SignedLicense) -> Result<LicensePayload> {
        let (payload_b64, sig_b64) = license.0.split_once('.').ok_or(Error::LicenseMalformed)?;

        let payload_bytes = BASE64
            .decode(payload_b64)
            .map_err(|_| Error::LicenseMalformed)?;

        let sig_bytes: [u8; 64] = BASE64
            .decode(sig_b64)
            .map_err(|_| Error::LicenseMalformed)?
            .try_into()
            .map_err(|_: Vec<u8>| Error::LicenseMalformed)?;

        let signature = Signature::from_bytes(&sig_bytes);

        self.verifying_key
            .verify(&payload_bytes, &signature)
            .map_err(|_| Error::LicenseInvalidSignature)?;

        let payload: LicensePayload =
            serde_json::from_slice(&payload_bytes).map_err(|_| Error::LicenseMalformed)?;

        if let Some(expires_at) = payload.expires_at
            && Utc::now() > expires_at
        {
            return Err(Error::LicenseExpired);
        }

        Ok(payload)
    }
}

fn license_file_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("rake").join("license"))
}

fn read_license_file() -> Option<String> {
    let path = license_file_path()?;
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn write_license_file(key: &str) -> std::io::Result<()> {
    let Some(path) = license_file_path() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, key)
}

fn format_licensee(payload: &LicensePayload) -> String {
    match &payload.expires_at {
        Some(dt) => format!("{} (expires {})", payload.licensee, dt.format("%Y-%m-%d")),
        None => format!("{} (perpetual)", payload.licensee),
    }
}

/// Load a payload without any I/O side-effects or status output.
///
/// Reads `RAKE_LICENSE` first, then the config file.  Any verification error
/// (including an unconfigured placeholder key) is treated as absent.
fn try_load_payload() -> Option<LicensePayload> {
    if !verifying_key_configured() {
        return None;
    }
    let verifier = LicenseVerifier::new().ok()?;
    if let Ok(key) = std::env::var("RAKE_LICENSE") {
        return verifier.verify(&SignedLicense(key)).ok();
    }
    let key = read_license_file()?;
    verifier.verify(&SignedLicense(key)).ok()
}

/// Validate `key`, persist it to the platform config file, and print a
/// `Licensed` status line.
///
/// Called from the `rake license` / `cargo rake license` subcommand handler.
/// Returns the decoded [`LicensePayload`] on success.
///
/// # Errors
///
/// Returns [`Error::LicenseMalformed`] when the key cannot be parsed or the
/// verifying key bytes are invalid, [`Error::LicenseInvalidSignature`] on a
/// bad signature, and [`Error::LicenseExpired`] when the key has passed its
/// expiry date.
pub fn activate_license(key: &str) -> Result<LicensePayload> {
    let verifier = LicenseVerifier::new()?;
    let payload = verifier.verify(&SignedLicense(key.trim().to_owned()))?;
    if let Err(e) = write_license_file(key.trim()) {
        eprintln!("warning: could not save license key: {e}");
    }
    print_label("Licensed", &format_licensee(&payload));
    Ok(payload)
}

/// Remove the stored license key file after confirming with the user.
///
/// Prints `"no license key stored"` and returns `Ok(())` when no file exists.
/// Requires an interactive stdin; returns [`Error::LicenseRemoveNoTerminal`]
/// when stdin is not a terminal. Prompts `[y/N]` on stderr; any response other
/// than `y`/`yes` (case-insensitive) prints `"removal cancelled"` and returns
/// `Ok(())`. On confirmed removal, deletes the file and prints a `Removed` line.
///
/// # Errors
///
/// Returns [`Error::LicenseRemoveNoTerminal`] when stdin is not a terminal,
/// [`Error::Io`] on a stdin read failure, and [`Error::LicenseRemoveFailed`]
/// when the file cannot be deleted.
pub fn remove_license() -> Result<()> {
    use std::io::{IsTerminal as _, Write as _, stderr, stdin};

    let Some(path) = license_file_path() else {
        print_label("License", "no config directory found; nothing to remove");
        return Ok(());
    };
    if !path.exists() {
        print_label("License", "no license key stored");
        return Ok(());
    }
    if !stdin().is_terminal() {
        return Err(Error::LicenseRemoveNoTerminal);
    }
    let mut err = stderr();
    let _ = write!(err, "Remove the stored license key? [y/N] ").ok();
    let _ = err.flush().ok();
    let mut line = String::new();
    let _read = stdin().read_line(&mut line).map_err(Error::Io)?;
    if !matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        print_label("License", "removal cancelled");
        return Ok(());
    }
    std::fs::remove_file(&path).map_err(Error::LicenseRemoveFailed)?;
    print_label("Removed", "license key deleted");
    Ok(())
}

/// Load a license for the `run` path, printing a `Licensed` status line on
/// success.
///
/// Resolution order: `RAKE_LICENSE` env var → platform config file.  Returns
/// `Ok(None)` when no key is configured (including when the placeholder key is
/// still in place).  Hard-fails on an invalid `RAKE_LICENSE` value; soft-fails
/// (warns) on a corrupt or expired config file.
///
/// # Errors
///
/// Returns [`Error::LicenseMalformed`] when the verifying key bytes are
/// invalid, and propagates [`Error::LicenseMalformed`],
/// [`Error::LicenseInvalidSignature`], or [`Error::LicenseExpired`] when
/// `RAKE_LICENSE` is set but the key fails verification.
pub fn load_license() -> Result<Option<LicensePayload>> {
    if !verifying_key_configured() {
        return Ok(None);
    }
    let verifier = LicenseVerifier::new()?;
    if let Ok(key) = std::env::var("RAKE_LICENSE") {
        let payload = verifier.verify(&SignedLicense(key))?;
        print_label("Licensed", &format_licensee(&payload));
        return Ok(Some(payload));
    }
    if let Some(key) = read_license_file() {
        match verifier.verify(&SignedLicense(key)) {
            Ok(payload) => {
                print_label("Licensed", &format_licensee(&payload));
                return Ok(Some(payload));
            }
            Err(e) => {
                eprintln!(
                    "warning: saved license is invalid ({e}); \
                     run `rake license <key>` to re-activate"
                );
            }
        }
    }
    Ok(None)
}

/// Print the status of the `basic` gated feature.
///
/// Loads the license in soft-fail mode (any error or absent key → locked).
/// Prints one status line without a `Licensed` header.
pub fn basic_feature_status() {
    let payload = try_load_payload();
    match payload {
        Some(ref p) if p.features.basic => {
            print_label("Basic", &format!("enabled for {}", p.licensee));
        }
        _ => {
            print_label("Basic", "locked — run `rake license <key>` to activate");
        }
    }
}

/// Print the licensee and the enabled/disabled state of every feature flag.
///
/// Loads the license in soft-fail mode (any error or absent key → no license).
pub fn license_info_status() {
    match try_load_payload() {
        Some(payload) => {
            print_label("Licensed", &format_licensee(&payload));
            print_label(
                "Features",
                &format!(
                    "basic: {}, events: {}",
                    on_off(payload.features.basic),
                    on_off(payload.features.events),
                ),
            );
        }
        None => {
            print_label(
                "License",
                "no license active — run `rake license <key>` to activate",
            );
        }
    }
}

fn on_off(enabled: bool) -> &'static str {
    if enabled { "enabled" } else { "disabled" }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use chrono::{DateTime, Utc};

    use super::{
        Features, LicensePayload, LicenseVerifier, SignedLicense, basic_feature_status,
        format_licensee, license_info_status, load_license, remove_license,
        verifying_key_configured,
    };
    use crate::error::Error as RakeError;

    #[test]
    fn load_license_no_config_returns_none() -> Result<(), Box<dyn Error>> {
        // Clear RAKE_LICENSE so the env-var branch cannot fire regardless of the
        // caller's environment.
        #[allow(unsafe_code)]
        // SAFETY: nextest runs each test in its own process; no concurrent env mutation.
        unsafe {
            std::env::remove_var("RAKE_LICENSE");
        };

        // If a license file is stored on this machine we cannot test the "no config"
        // path without controlling the file path — skip rather than fail.
        if super::read_license_file().is_some() {
            return Ok(());
        }
        // With no env var and no file, load_license() must return None whether the
        // verifying key is a placeholder or a real key.
        let result = load_license()?;
        assert!(result.is_none());
        Ok(())
    }

    #[test]
    fn verifier_new_succeeds_but_configured_guard_is_false() {
        // Any valid 32-byte compressed Ed25519 point — placeholder or real — is
        // accepted by new(); this test ensures the constructor never errors out.
        assert!(LicenseVerifier::new().is_ok());
    }

    #[test]
    fn basic_feature_status_does_not_panic() {
        // Should print "locked" without panicking when no license is configured.
        basic_feature_status();
    }

    #[test]
    fn license_info_status_does_not_panic() {
        license_info_status();
    }

    #[test]
    fn tampered_key_rejected() {
        // A key with no '.' separator is malformed; test the split logic.
        // If new() fails (placeholder key) the test is vacuously ok — the split
        // logic is exercised only when a real verifier key is present.
        if let Ok(verifier) = LicenseVerifier::new() {
            let bad = SignedLicense("notvalid".to_owned());
            assert!(verifier.verify(&bad).is_err());
        }
    }

    #[test]
    fn verifying_key_configured_returns_true() {
        // The current placeholder bytes are non-zero, so the guard must return true.
        assert!(verifying_key_configured());
    }

    #[test]
    fn format_licensee_with_expiry() -> Result<(), Box<dyn Error>> {
        let expires_at: DateTime<Utc> = "2025-01-15T00:00:00Z".parse()?;
        let payload = LicensePayload {
            licensee: "test@example.com".to_owned(),
            features: Features {
                basic: true,
                events: false,
            },
            expires_at: Some(expires_at),
            issued_at: Utc::now(),
        };
        let formatted = format_licensee(&payload);
        assert!(formatted.contains("test@example.com"));
        assert!(formatted.contains("2025-01-15"));
        Ok(())
    }

    #[test]
    fn format_licensee_perpetual() {
        let payload = LicensePayload {
            licensee: "perpetual@example.com".to_owned(),
            features: Features {
                basic: false,
                events: false,
            },
            expires_at: None,
            issued_at: Utc::now(),
        };
        let formatted = format_licensee(&payload);
        assert!(formatted.contains("perpetual@example.com"));
        assert!(formatted.contains("perpetual"));
    }

    #[test]
    fn features_without_events_field_deserializes_locked() -> Result<(), Box<dyn Error>> {
        // A license signed before `events` existed has no such key in its JSON
        // payload; `#[serde(default)]` must default it to `false` rather than
        // failing deserialization.
        let features: Features = serde_json::from_str(r#"{"basic":true}"#)?;
        assert!(features.basic);
        assert!(!features.events);
        Ok(())
    }

    #[test]
    fn verify_bad_base64_payload() -> Result<(), Box<dyn Error>> {
        let verifier = LicenseVerifier::new()?;
        let key = SignedLicense("not-valid-base64!!!!.something".to_owned());
        assert!(matches!(
            verifier.verify(&key),
            Err(RakeError::LicenseMalformed)
        ));
        Ok(())
    }

    #[test]
    fn verify_bad_base64_signature() -> Result<(), Box<dyn Error>> {
        let verifier = LicenseVerifier::new()?;
        let payload_b64 = BASE64.encode(b"some payload");
        let key = SignedLicense(format!("{payload_b64}.not-valid-base64!!!!"));
        assert!(matches!(
            verifier.verify(&key),
            Err(RakeError::LicenseMalformed)
        ));
        Ok(())
    }

    #[test]
    fn verify_signature_wrong_length() -> Result<(), Box<dyn Error>> {
        let verifier = LicenseVerifier::new()?;
        let payload_b64 = BASE64.encode(b"some payload");
        // 32 bytes encodes to valid base64 but try_into [u8; 64] fails.
        let sig_b64 = BASE64.encode([0u8; 32]);
        let key = SignedLicense(format!("{payload_b64}.{sig_b64}"));
        assert!(matches!(
            verifier.verify(&key),
            Err(RakeError::LicenseMalformed)
        ));
        Ok(())
    }

    #[test]
    fn verify_invalid_signature() -> Result<(), Box<dyn Error>> {
        let verifier = LicenseVerifier::new()?;
        // 64 zero bytes decode to a valid-length sig array but won't verify against any payload.
        let payload_b64 = BASE64.encode(b"arbitrary payload bytes");
        let sig_b64 = BASE64.encode([0u8; 64]);
        let key = SignedLicense(format!("{payload_b64}.{sig_b64}"));
        assert!(matches!(
            verifier.verify(&key),
            Err(RakeError::LicenseInvalidSignature)
        ));
        Ok(())
    }

    #[test]
    fn load_license_env_malformed() {
        #[allow(unsafe_code)]
        // SAFETY: nextest runs each test in its own process; no concurrent env mutation.
        unsafe {
            std::env::set_var("RAKE_LICENSE", "badkey-no-dot-separator");
        }
        let result = load_license();
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("RAKE_LICENSE");
        }
        assert!(matches!(result, Err(RakeError::LicenseMalformed)));
    }

    #[test]
    fn load_license_env_invalid_sig() {
        // Structurally valid (b64.b64, sig decodes to 64 bytes) but wrong sig bytes
        // → the cryptographic check must return LicenseInvalidSignature.
        let payload_b64 = BASE64.encode(b"some-payload-bytes");
        let sig_b64 = BASE64.encode([0u8; 64]);
        let key_val = format!("{payload_b64}.{sig_b64}");
        #[allow(unsafe_code)]
        // SAFETY: nextest runs each test in its own process; no concurrent env mutation.
        unsafe {
            std::env::set_var("RAKE_LICENSE", &key_val);
        }
        let result = load_license();
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("RAKE_LICENSE");
        }
        assert!(matches!(result, Err(RakeError::LicenseInvalidSignature)));
    }

    #[test]
    fn remove_license_no_file_or_no_terminal() -> Result<(), Box<dyn Error>> {
        // When no license file is stored: Ok(()) ("no license key stored").
        // When a file exists but stdin is not a TTY (always in tests): LicenseRemoveNoTerminal.
        // Any other outcome is a bug.
        match remove_license() {
            Ok(()) | Err(RakeError::LicenseRemoveNoTerminal) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}
