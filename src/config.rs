//! Profiles directory, `profiles.yaml` mapping, and import/delete.
//!
//! Spec §2 layout under [`tuiblick_dir()`] (`~/.config/tuiblick`):
//! `awg/<sanitized>.conf`, `openvpn/<sanitized>.<ovpn|conf>`, plus a
//! `profiles.yaml` list mapping display names to stored files.
//!
//! The public API works against the real config root. The private
//! `*_in(root)` variants take an explicit root so unit tests can use
//! unique temp dirs under `std::env::temp_dir()` (std-only, no
//! `tempfile` dev-dependency). `TUIBLICK_CONFIG_DIR` overrides the root,
//! reserved for future integration tests — never set in production.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::SecondsFormat;
use serde::{Deserialize, Serialize};

use crate::errors::AppError;

/// VPN backend a profile belongs to.
///
/// Serialized exactly as spec §2 requires: `awg` / `openvpn`
/// (note: plain `snake_case` would yield `open_vpn`, hence explicit renames).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VpnType {
    #[serde(rename = "awg")]
    Awg,
    #[serde(rename = "openvpn")]
    OpenVpn,
}

impl VpnType {
    /// Config subdirectory holding this backend's files.
    pub fn subdir(self) -> &'static str {
        match self {
            VpnType::Awg => "awg",
            VpnType::OpenVpn => "openvpn",
        }
    }

    /// Extension used when the import source has none usable.
    pub fn default_extension(self) -> &'static str {
        match self {
            VpnType::Awg => "conf",
            VpnType::OpenVpn => "ovpn",
        }
    }

    fn allows_extension(self, ext: &str) -> bool {
        match self {
            VpnType::Awg => ext == "conf",
            VpnType::OpenVpn => ext == "ovpn" || ext == "conf",
        }
    }
}

/// One row of `profiles.yaml` (spec §2 schema).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub display_name: String,
    pub vpn_type: VpnType,
    pub stored_file: String,
    pub original_filename: String,
    pub created_at: String,
}

impl Profile {
    fn stored_path_in(&self, root: &Path) -> PathBuf {
        root.join(&self.stored_file)
    }

    /// Absolute path of the stored config file.
    pub fn stored_path(&self) -> PathBuf {
        self.stored_path_in(&config_root())
    }

    fn is_missing_in(&self, root: &Path) -> bool {
        !self.stored_path_in(root).is_file()
    }

    /// `true` when the mapped file is absent on disk.
    ///
    /// Such entries are kept in the mapping and shown as `(missing)`.
    pub fn is_missing(&self) -> bool {
        self.is_missing_in(&config_root())
    }
}

/// Config root: `$TUIBLICK_CONFIG_DIR` override, else `~/.config/tuiblick`.
fn config_root() -> PathBuf {
    if let Ok(dir) = std::env::var("TUIBLICK_CONFIG_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    dirs::config_dir()
        .map(|d| d.join("tuiblick"))
        .unwrap_or_else(|| std::env::temp_dir().join("tuiblick"))
}

/// Absolute config directory (`~/.config/tuiblick` on macOS).
/// Spec Task 2 interface; the app entry points use `scan_and_reconcile` /
/// `import_file` / `delete_profile`, so this stays for API completeness.
#[allow(dead_code)]
pub fn tuiblick_dir() -> PathBuf {
    config_root()
}

fn profiles_path_in(root: &Path) -> PathBuf {
    root.join("profiles.yaml")
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Sanitize a filename stem: lowercase, `[^a-z0-9-_]` → `-`, collapse
/// dashes, trim them, fall back to `"profile"` when nothing remains.
pub fn sanitize_filename(stem: &str) -> String {
    let mut out = String::with_capacity(stem.len());
    let mut prev_dash = false;
    for ch in stem.to_lowercase().chars() {
        let keep = ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-';
        let c = if keep { ch } else { '-' };
        if c == '-' {
            if !prev_dash {
                out.push('-');
            }
            prev_dash = true;
        } else {
            out.push(c);
            prev_dash = false;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "profile".to_string()
    } else {
        trimmed.to_string()
    }
}

fn load_profiles_in(root: &Path) -> Result<Vec<Profile>, AppError> {
    let path = profiles_path_in(root);
    match fs::read_to_string(&path) {
        Ok(text) => {
            if text.trim().is_empty() {
                return Ok(Vec::new());
            }
            serde_yaml::from_str(&text)
                .map_err(|e| AppError::Config(format!("cannot parse {}: {e}", path.display())))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(AppError::Io(e)),
    }
}

/// Load the profile mapping; a missing `profiles.yaml` means empty, not error.
/// Spec Task 2 interface; live code paths go through `scan_and_reconcile`.
#[allow(dead_code)]
pub fn load_profiles() -> Result<Vec<Profile>, AppError> {
    load_profiles_in(&config_root())
}

fn save_profiles_in(root: &Path, profiles: &[Profile]) -> Result<(), AppError> {
    fs::create_dir_all(root)?;
    let text = serde_yaml::to_string(profiles)
        .map_err(|e| AppError::Config(format!("cannot encode profiles.yaml: {e}")))?;
    fs::write(profiles_path_in(root), text)?;
    Ok(())
}

/// Persist the profile mapping (creates the config dir if needed).
/// Spec Task 2 interface; live code paths go through `scan_and_reconcile`.
#[allow(dead_code)]
pub fn save_profiles(profiles: &[Profile]) -> Result<(), AppError> {
    save_profiles_in(&config_root(), profiles)
}

/// `(vpn_type, stored_file)` pairs present on disk, sorted for determinism.
fn scan_disk(root: &Path) -> Vec<(VpnType, String)> {
    let mut found = Vec::new();
    for vpn in [VpnType::Awg, VpnType::OpenVpn] {
        let dir = root.join(vpn.subdir());
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        let mut names: Vec<String> = entries
            .filter_map(|entry| {
                let entry = entry.ok()?;
                if !entry.file_type().ok()?.is_file() {
                    return None;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                let wanted = match vpn {
                    VpnType::Awg => name.ends_with(".conf"),
                    VpnType::OpenVpn => name.ends_with(".ovpn") || name.ends_with(".conf"),
                };
                wanted.then_some(name)
            })
            .collect();
        names.sort();
        for name in names {
            found.push((vpn, format!("{}/{}", vpn.subdir(), name)));
        }
    }
    found
}

fn scan_and_reconcile_in(root: &Path) -> Result<Vec<Profile>, AppError> {
    fs::create_dir_all(root.join(VpnType::Awg.subdir()))?;
    fs::create_dir_all(root.join(VpnType::OpenVpn.subdir()))?;
    let mut profiles = load_profiles_in(root)?;
    let known: HashSet<String> = profiles.iter().map(|p| p.stored_file.clone()).collect();
    let mut changed = false;
    for (vpn, stored) in scan_disk(root) {
        if known.contains(stored.as_str()) {
            continue;
        }
        let filename = stored
            .rsplit('/')
            .next()
            .unwrap_or(stored.as_str())
            .to_string();
        let stem = filename
            .rsplit_once('.')
            .map_or(filename.clone(), |(stem, _)| stem.to_string());
        profiles.push(Profile {
            id: uuid::Uuid::new_v4().to_string(),
            display_name: stem.to_string(),
            vpn_type: vpn,
            stored_file: stored,
            original_filename: filename.to_string(),
            created_at: now_rfc3339(),
        });
        changed = true;
    }
    if changed {
        save_profiles_in(root, &profiles)?;
    }
    Ok(profiles)
}

/// Scan `awg/*.conf` + `openvpn/*.{ovpn,conf}`; entries pointing at
/// missing files are kept (shown as `(missing)`), files on disk with
/// no mapping are auto-registered with display name = file stem.
pub fn scan_and_reconcile() -> Result<Vec<Profile>, AppError> {
    scan_and_reconcile_in(&config_root())
}

/// Expand a leading `~` against the home directory; anything else passes through.
fn expand_tilde(path: &Path) -> PathBuf {
    if let Some(rest) = path.to_str().and_then(|s| s.strip_prefix('~')) {
        if rest.is_empty() || rest.starts_with('/') {
            if let Some(home) = dirs::home_dir() {
                return home.join(rest.trim_start_matches('/'));
            }
        }
    }
    path.to_path_buf()
}

fn import_file_in(root: &Path, src: &Path, vpn: VpnType) -> Result<Profile, AppError> {
    let src = expand_tilde(src);
    if !src.is_file() {
        return Err(AppError::Config(format!(
            "source file not found: {}",
            src.display()
        )));
    }
    let original_filename = src
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::Config(format!("bad source filename: {}", src.display())))?;
    let (stem_raw, ext_raw) = match original_filename.rsplit_once('.') {
        Some((stem, ext)) => (stem, Some(ext)),
        None => (original_filename.as_str(), None),
    };
    let profiles = load_profiles_in(root)?;
    let taken: HashSet<&str> = profiles.iter().map(|p| p.stored_file.as_str()).collect();
    let dir = root.join(vpn.subdir());
    fs::create_dir_all(&dir)?;
    // Stored filename + list name depend on the backend. `awg-quick up <path>`
    // derives the interface name from the file stem, which must be a valid
    // interface name (short, <=15 chars) — so AWG configs are stored as
    // `awgN.conf` while the list keeps the human-readable original stem.
    // `openvpn --config` accepts any path, so OpenVPN keeps the sanitized name.
    let (candidate, display_name) = match vpn {
        VpnType::Awg => {
            let mut candidate = String::new();
            for n in 0u32.. {
                let c = format!("awg{n}.conf");
                if !dir.join(&c).exists() && !taken.contains(stored_name(vpn, &c).as_str()) {
                    candidate = c;
                    break;
                }
            }
            (candidate, stem_raw.to_owned())
        }
        VpnType::OpenVpn => {
            let base = sanitize_filename(stem_raw);
            let ext = match ext_raw.map(str::to_lowercase) {
                Some(ext) if vpn.allows_extension(&ext) => ext,
                _ => vpn.default_extension().to_string(),
            };
            let mut candidate = format!("{base}.{ext}");
            let mut n = 2;
            while dir.join(&candidate).exists()
                || taken.contains(stored_name(vpn, &candidate).as_str())
            {
                candidate = format!("{base}-{n}.{ext}");
                n += 1;
            }
            let stem = candidate
                .rsplit_once('.')
                .map_or(candidate.clone(), |(stem, _)| stem.to_string());
            (candidate, stem)
        }
    };
    let rel = stored_name(vpn, &candidate);
    fs::copy(&src, dir.join(&candidate))?;
    let profile = Profile {
        id: uuid::Uuid::new_v4().to_string(),
        display_name,
        vpn_type: vpn,
        stored_file: rel,
        original_filename,
        created_at: now_rfc3339(),
    };
    let mut profiles = profiles;
    profiles.push(profile.clone());
    save_profiles_in(root, &profiles)?;
    Ok(profile)
}

fn stored_name(vpn: VpnType, filename: &str) -> String {
    format!("{}/{}", vpn.subdir(), filename)
}

/// Copy `src` (absolute or `~`-prefixed) into the backend subdir and append
/// the mapping, returning the new profile. AWG files are stored as short
/// interface-safe `awgN.conf` names (the list keeps the original stem);
/// OpenVPN files keep the sanitized name.
/// The source file is never moved or deleted.
pub fn import_file(src: &Path, vpn: VpnType) -> Result<Profile, AppError> {
    import_file_in(&config_root(), src, vpn)
}

fn delete_profile_in(root: &Path, id: &str) -> Result<(), AppError> {
    let mut profiles = load_profiles_in(root)?;
    let pos = profiles
        .iter()
        .position(|p| p.id == id)
        .ok_or_else(|| AppError::Config(format!("profile not found: {id}")))?;
    let removed = profiles.remove(pos);
    match fs::remove_file(root.join(&removed.stored_file)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(AppError::Io(e)),
    }
    save_profiles_in(root, &profiles)?;
    Ok(())
}

/// Remove the stored file (if present) and its mapping entry.
pub fn delete_profile(id: &str) -> Result<(), AppError> {
    delete_profile_in(&config_root(), id)
}

fn rename_profile_in(root: &Path, id: &str, new_name: &str) -> Result<(), AppError> {
    let new_name = new_name.trim();
    if new_name.is_empty() {
        return Err(AppError::Config("profile name is empty".to_owned()));
    }
    let mut profiles = load_profiles_in(root)?;
    let profile = profiles
        .iter_mut()
        .find(|p| p.id == id)
        .ok_or_else(|| AppError::Config(format!("profile not found: {id}")))?;
    profile.display_name = new_name.to_owned();
    save_profiles_in(root, &profiles)?;
    Ok(())
}

/// Rename a profile's display name (stored file untouched).
pub fn rename_profile(id: &str, new_name: &str) -> Result<(), AppError> {
    rename_profile_in(&config_root(), id, new_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Unique scratch root per test (std-only, no `tempfile` needed).
    fn test_root(tag: &str) -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "tuiblick-config-test-{}-{tag}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create test root");
        dir
    }

    fn cleanup(dir: &Path) {
        let _ = fs::remove_dir_all(dir);
    }

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        fs::write(path, contents).expect("write fixture file");
    }

    #[test]
    fn sanitize_lowercases_and_dashes() {
        assert_eq!(sanitize_filename("My Office (1)"), "my-office-1");
    }

    #[test]
    fn sanitize_edge_cases() {
        assert_eq!(sanitize_filename("a---b"), "a-b");
        assert_eq!(sanitize_filename("--Foo__Bar--"), "foo__bar");
        assert_eq!(sanitize_filename("already-fine_123"), "already-fine_123");
        assert_eq!(sanitize_filename(""), "profile");
        assert_eq!(sanitize_filename("((()))"), "profile");
    }

    #[test]
    fn vpn_type_yaml_spelling_matches_spec() {
        assert_eq!(serde_yaml::to_string(&VpnType::Awg).unwrap().trim(), "awg");
        assert_eq!(
            serde_yaml::to_string(&VpnType::OpenVpn).unwrap().trim(),
            "openvpn"
        );
        assert_eq!(
            serde_yaml::from_str::<VpnType>("openvpn").unwrap(),
            VpnType::OpenVpn
        );
    }

    #[test]
    fn load_missing_yaml_returns_empty() {
        let root = test_root("empty");
        let profiles = load_profiles_in(&root).expect("missing yaml is not an error");
        assert!(profiles.is_empty());
        cleanup(&root);
    }

    #[test]
    fn save_load_round_trip() {
        let root = test_root("roundtrip");
        let profile = Profile {
            id: uuid::Uuid::new_v4().to_string(),
            display_name: "my-office".to_string(),
            vpn_type: VpnType::OpenVpn,
            stored_file: "openvpn/my-office.ovpn".to_string(),
            original_filename: "My Office (1).ovpn".to_string(),
            created_at: now_rfc3339(),
        };
        save_profiles_in(&root, std::slice::from_ref(&profile)).expect("save");
        let back = load_profiles_in(&root).expect("reload");
        assert_eq!(back, vec![profile]);
        cleanup(&root);
    }

    #[test]
    fn scan_registers_orphan_files() {
        let root = test_root("scan");
        write_file(&root.join("awg/office.conf"), "wg fixture");
        write_file(&root.join("openvpn/tun.ovpn"), "client");
        write_file(&root.join("openvpn/tun2.conf"), "client");
        write_file(&root.join("openvpn/notes.txt"), "ignored");
        let profiles = scan_and_reconcile_in(&root).expect("scan");
        assert_eq!(profiles.len(), 3);
        let awg = profiles
            .iter()
            .find(|p| p.vpn_type == VpnType::Awg)
            .expect("awg entry");
        assert_eq!(awg.display_name, "office");
        assert_eq!(awg.stored_file, "awg/office.conf");
        assert_eq!(awg.original_filename, "office.conf");
        assert!(!awg.id.is_empty());
        // Persisted and idempotent: rescan registers nothing new.
        let reloaded = load_profiles_in(&root).expect("reload");
        assert_eq!(reloaded.len(), 3);
        let again = scan_and_reconcile_in(&root).expect("rescan");
        assert_eq!(again.len(), 3);
        cleanup(&root);
    }

    #[test]
    fn import_keeps_original_filename() {
        let root = test_root("import");
        let srcdir = test_root("import-src");
        let src = srcdir.join("My Office (1).conf");
        write_file(&src, "wg fixture");
        let p = import_file_in(&root, &src, VpnType::Awg).expect("import");
        assert_eq!(p.original_filename, "My Office (1).conf");
        assert_eq!(p.display_name, "My Office (1)");
        assert_eq!(p.stored_file, "awg/awg0.conf");
        assert_eq!(p.vpn_type, VpnType::Awg);
        assert!(src.is_file(), "import copies, never moves");
        assert_eq!(
            fs::read_to_string(root.join("awg/awg0.conf")).unwrap(),
            "wg fixture"
        );
        let mapped = load_profiles_in(&root).expect("reload");
        assert_eq!(mapped.len(), 1);
        assert_eq!(mapped[0].id, p.id);
        cleanup(&root);
        cleanup(&srcdir);
    }

    #[test]
    fn import_dedupes_with_numeric_suffix() {
        let root = test_root("dedupe");
        let srcdir = test_root("dedupe-src");
        let first = srcdir.join("sub1").join("dup.conf");
        let second = srcdir.join("sub2").join("dup.conf");
        write_file(&first, "one");
        write_file(&second, "two");
        let a = import_file_in(&root, &first, VpnType::Awg).expect("import 1");
        let b = import_file_in(&root, &second, VpnType::Awg).expect("import 2");
        assert_eq!(a.stored_file, "awg/awg0.conf");
        assert_eq!(b.stored_file, "awg/awg1.conf");
        assert_eq!(a.display_name, "dup");
        assert_eq!(b.display_name, "dup");
        assert_eq!(
            fs::read_to_string(root.join("awg/awg0.conf")).unwrap(),
            "one"
        );
        assert_eq!(
            fs::read_to_string(root.join("awg/awg1.conf")).unwrap(),
            "two"
        );
        cleanup(&root);
        cleanup(&srcdir);
    }

    #[test]
    fn awg_import_uses_short_interface_safe_name() {
        // `awg-quick up <path>` derives the interface name from the file
        // stem: a long sanitized name fails at connect time, so AWG imports
        // are stored as awgN.conf (see reported exmo-...-dev.conf failure).
        let root = test_root("awg-iface");
        let srcdir = test_root("awg-iface-src");
        let src = srcdir.join("exmo-amnezia-20251024-aleksey_mikhailov-dev.conf");
        write_file(&src, "wg fixture");
        let p = import_file_in(&root, &src, VpnType::Awg).expect("import");
        assert_eq!(p.stored_file, "awg/awg0.conf");
        assert_eq!(
            p.display_name,
            "exmo-amnezia-20251024-aleksey_mikhailov-dev"
        );
        let stem = "awg0";
        assert!(stem.len() <= 15);
        assert!(
            stem.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
            "stem must be interface-safe"
        );
        cleanup(&root);
        cleanup(&srcdir);
    }

    #[test]
    fn import_missing_source_is_error() {
        let root = test_root("import-missing");
        let err = import_file_in(&root, &root.join("nope.conf"), VpnType::Awg)
            .expect_err("missing source must fail");
        assert!(matches!(err, AppError::Config(_)));
        cleanup(&root);
    }

    #[test]
    fn missing_file_entry_is_flagged_not_dropped() {
        let root = test_root("missing");
        let profile = Profile {
            id: "x".to_string(),
            display_name: "gone".to_string(),
            vpn_type: VpnType::Awg,
            stored_file: "awg/gone.conf".to_string(),
            original_filename: "gone.conf".to_string(),
            created_at: now_rfc3339(),
        };
        save_profiles_in(&root, std::slice::from_ref(&profile)).expect("save");
        let profiles = scan_and_reconcile_in(&root).expect("scan");
        assert_eq!(profiles.len(), 1, "missing entries are kept");
        assert!(profiles[0].is_missing_in(&root));
        cleanup(&root);
    }

    #[test]
    fn delete_removes_file_and_mapping() {
        let root = test_root("delete");
        let srcdir = test_root("delete-src");
        let src = srcdir.join("gone.conf");
        write_file(&src, "wg fixture");
        let p = import_file_in(&root, &src, VpnType::Awg).expect("import");
        delete_profile_in(&root, &p.id).expect("delete");
        assert!(!root.join(&p.stored_file).exists());
        assert!(load_profiles_in(&root).expect("reload").is_empty());
        let err = delete_profile_in(&root, &p.id).expect_err("second delete must fail");
        assert!(matches!(err, AppError::Config(_)));
        cleanup(&root);
        cleanup(&srcdir);
    }

    #[test]
    fn rename_updates_display_name_only() {
        let root = test_root("rename");
        let profile = Profile {
            id: "r1".to_string(),
            display_name: "old".to_string(),
            vpn_type: VpnType::Awg,
            stored_file: "awg/old.conf".to_string(),
            original_filename: "old.conf".to_string(),
            created_at: now_rfc3339(),
        };
        save_profiles_in(&root, std::slice::from_ref(&profile)).expect("save");
        rename_profile_in(&root, "r1", "  New Name  ").expect("rename");
        let back = load_profiles_in(&root).expect("reload");
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].display_name, "New Name");
        assert_eq!(back[0].stored_file, "awg/old.conf");
        assert_eq!(back[0].original_filename, "old.conf");
        cleanup(&root);
    }

    #[test]
    fn rename_rejects_blank_and_unknown_id() {
        let root = test_root("rename-err");
        let err = rename_profile_in(&root, "nope", "x").expect_err("unknown id must fail");
        assert!(matches!(err, AppError::Config(_)));
        let err = rename_profile_in(&root, "nope", "   ").expect_err("blank name must fail");
        assert!(matches!(err, AppError::Config(_)));
        cleanup(&root);
    }
}
