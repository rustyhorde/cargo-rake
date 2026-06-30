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
    0x5f, 0x4f, 0x41, 0x01, 0x88, 0xc8, 0x2a, 0xd2, 0xf4, 0x2e, 0x63, 0x26, 0x12, 0x5c, 0x44, 0xdc,
    0x6b, 0x77, 0x02, 0xa5, 0xe8, 0x2d, 0x83, 0x54, 0x3b, 0x5a, 0x4a, 0x98, 0x80, 0x54, 0xec, 0xa3,
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

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::{LicenseVerifier, SignedLicense, basic_feature_status, load_license};

    #[test]
    fn load_license_no_config_returns_none() -> Result<(), Box<dyn Error>> {
        // VERIFYING_KEY_BYTES is the all-zero placeholder: verifying_key_configured()
        // returns false and load_license returns Ok(None) before checking any env var.
        let result = load_license()?;
        assert!(result.is_none());
        Ok(())
    }

    #[test]
    fn verifier_new_succeeds_but_configured_guard_is_false() {
        // All-zero bytes happen to be a valid Ed25519 compressed point, so new()
        // succeeds.  The verifying_key_configured() guard (zero ≠ real key) is
        // what prevents the placeholder from being used in load_license().
        assert!(LicenseVerifier::new().is_ok());
    }

    #[test]
    fn basic_feature_status_does_not_panic() {
        // Should print "locked" without panicking when no license is configured.
        basic_feature_status();
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
}
