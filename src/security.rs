// Modified in hyperdesk from upstream bahmgit/hlcli (Apache-2.0): redacted Credentials Debug, enforced KDF floors, added 0600 password-file reader, and added signer collision guard.
use std::{
    fmt, fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use alloy_primitives::Address;
use alloy_signer_local::PrivateKeySigner;
use argon2::Argon2;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

pub const MIN_KDF_MEM_KIB: u32 = 19_456;
pub const MIN_KDF_ITERS: u32 = 2;

#[derive(Clone, Serialize, Deserialize, Zeroize)]
#[zeroize(drop)]
pub struct Credentials {
    pub main_wallet: String,
    pub api_private_key: String,
    pub network: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("main_wallet", &self.main_wallet)
            .field("api_private_key", &"[REDACTED]")
            .field("network", &self.network)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WalletProfilesIndex {
    pub version: u8,
    pub default_profile: String,
    pub profiles: Vec<WalletProfileRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WalletProfileRecord {
    pub id: String,
    pub label: String,
    pub main_wallet: Option<String>,
    pub network: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

pub fn load_credentials(
    data_dir: &Path,
    profile: Option<&str>,
    password: &[u8],
) -> anyhow::Result<Credentials> {
    let profile = selected_profile_id(data_dir, profile)?;
    let (cred_path, salt_path) = credential_paths(data_dir, &profile);
    let salt = std::fs::read(&salt_path)?;
    anyhow::ensure!(salt.len() >= 16, "invalid credential salt");
    let mut key = derive_key(password, &salt)?;
    let data = std::fs::read(&cred_path)?;
    anyhow::ensure!(data.len() >= 12, "invalid credential blob");
    let nonce_bytes: [u8; 12] = data[..12].try_into()?;
    let ciphertext = &data[12..];
    let cipher = match Aes256Gcm::new_from_slice(&key) {
        Ok(cipher) => cipher,
        Err(_) => {
            key.zeroize();
            anyhow::bail!("invalid derived credential key length");
        }
    };
    let mut plaintext = cipher
        .decrypt(&Nonce::from(nonce_bytes), ciphertext)
        .map_err(|err| {
            key.zeroize();
            anyhow::anyhow!("decrypt credentials: {err}")
        })?;
    key.zeroize();
    let credentials = serde_json::from_slice(&plaintext);
    plaintext.zeroize();
    credentials.map_err(Into::into)
}

pub fn selected_profile_id(data_dir: &Path, profile: Option<&str>) -> anyhow::Result<String> {
    match profile {
        Some(profile) => normalize_profile_id(profile),
        None => default_profile_id(data_dir),
    }
}

pub fn credentials_exist(data_dir: &Path, profile: &str) -> anyhow::Result<bool> {
    let profile = normalize_profile_id(profile)?;
    let (cred, salt) = credential_paths(data_dir, &profile);
    let cred_exists = cred.exists();
    let salt_exists = salt.exists();
    anyhow::ensure!(
        cred_exists == salt_exists,
        "incomplete credential files for wallet profile {profile}"
    );
    Ok(cred_exists)
}

pub fn validate_agent_signer(
    signer_addr: Address,
    main_wallet: &str,
    vault_address: Option<&str>,
) -> anyhow::Result<()> {
    let main_addr: Address = main_wallet
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid main wallet address"))?;
    anyhow::ensure!(
        signer_addr != main_addr,
        "refusing Master Wallet private key: API signer address {signer_addr} matches main_wallet {main_addr}; create a delegated Hyperliquid Agent Wallet instead"
    );
    if let Some(vault_raw) = vault_address {
        let vault_addr: Address = vault_raw
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid vault address"))?;
        anyhow::ensure!(
            signer_addr != vault_addr,
            "refusing Vault private key: API signer address {signer_addr} matches vaultAddress {vault_addr}; create a delegated Hyperliquid Agent Wallet instead"
        );
    }
    Ok(())
}

pub fn store_validated_credentials(
    data_dir: &Path,
    profile: &str,
    password: &[u8],
    credentials: &Credentials,
    vault_address: Option<&str>,
) -> anyhow::Result<()> {
    let signer: PrivateKeySigner = credentials
        .api_private_key
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid API private key"))?;
    validate_agent_signer(signer.address(), &credentials.main_wallet, vault_address)?;
    store_credentials(data_dir, profile, password, credentials)
}

pub fn read_password_file(path: &Path) -> anyhow::Result<Zeroizing<String>> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|err| anyhow::anyhow!("inspect password file {}: {err}", path.display()))?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "password file {} must be a regular file (not a symlink, directory, or FIFO)",
        path.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let mode = metadata.mode() & 0o777;
        anyhow::ensure!(
            mode & 0o077 == 0,
            "password file {} has insecure permissions {:04o}; expected 0600 (no group/other access)",
            path.display(),
            mode
        );
    }
    let mut raw = Zeroizing::new(
        fs::read_to_string(path)
            .map_err(|err| anyhow::anyhow!("read password file {}: {err}", path.display()))?,
    );
    let trimmed_len = raw.trim_end_matches(['\r', '\n']).len();
    raw.truncate(trimmed_len);
    Ok(raw)
}

pub fn store_credentials(
    data_dir: &Path,
    profile: &str,
    password: &[u8],
    credentials: &Credentials,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !password.is_empty(),
        "credentials password must not be empty"
    );
    let profile = normalize_profile_id(profile)?;
    secure_dir(data_dir)?;
    let (cred_path, salt_path) = profile_credential_paths(data_dir, &profile);
    if let Some(parent) = cred_path.parent() {
        secure_dir(parent)?;
    }
    let mut plaintext = serde_json::to_vec(credentials)?;
    let mut salt = [0_u8; 16];
    let mut nonce_bytes = [0_u8; 12];
    let mut key = [0_u8; 32];
    let mut blob = Vec::new();
    let result = (|| -> anyhow::Result<()> {
        getrandom::fill(&mut salt)
            .map_err(|err| anyhow::anyhow!("generate credential salt: {err}"))?;
        key = derive_key(password, &salt)?;
        let cipher = Aes256Gcm::new_from_slice(&key)
            .map_err(|_| anyhow::anyhow!("invalid derived credential key length"))?;
        getrandom::fill(&mut nonce_bytes)
            .map_err(|err| anyhow::anyhow!("generate credential nonce: {err}"))?;
        let ciphertext = cipher
            .encrypt(&Nonce::from(nonce_bytes), plaintext.as_ref())
            .map_err(|err| anyhow::anyhow!("encrypt credentials: {err}"))?;
        blob.reserve_exact(nonce_bytes.len() + ciphertext.len());
        blob.extend_from_slice(&nonce_bytes);
        blob.extend_from_slice(&ciphertext);
        write_secure_file(&salt_path, &salt)?;
        write_secure_file(&cred_path, &blob)?;
        upsert_profile_record(data_dir, &profile, credentials)
    })();
    plaintext.zeroize();
    key.zeroize();
    nonce_bytes.zeroize();
    salt.zeroize();
    blob.zeroize();
    result
}

pub fn default_profile_id(data_dir: &Path) -> anyhow::Result<String> {
    let path = data_dir.join("wallet_profiles.json");
    if !path.exists() {
        return Ok("default".to_string());
    }
    let index: WalletProfilesIndex = serde_json::from_slice(&std::fs::read(path)?)?;
    validate_profile_index(&index)?;
    normalize_profile_id(&index.default_profile)
}

pub fn normalize_profile_id(raw: &str) -> anyhow::Result<String> {
    let trimmed = raw.trim();
    anyhow::ensure!(!trimmed.is_empty(), "wallet profile id must not be empty");
    anyhow::ensure!(trimmed.len() <= 64, "wallet profile id too long");
    let normalized = trimmed.to_ascii_lowercase();
    anyhow::ensure!(
        normalized.chars().all(|ch| ch.is_ascii_lowercase()
            || ch.is_ascii_digit()
            || matches!(ch, '-' | '_' | '.')),
        "wallet profile id must match [a-z0-9._-]"
    );
    anyhow::ensure!(
        normalized != "." && normalized != "..",
        "wallet profile id must not be '.' or '..'"
    );
    Ok(normalized)
}

fn credential_paths(data_dir: &Path, profile: &str) -> (PathBuf, PathBuf) {
    let profile_dir = data_dir.join("wallets").join(profile);
    let cred = profile_dir.join("credentials.bin");
    let salt = profile_dir.join("salt.bin");
    if cred.exists() || salt.exists() || profile != "default" {
        return (cred, salt);
    }
    (data_dir.join("credentials.bin"), data_dir.join("salt.bin"))
}

fn profile_credential_paths(data_dir: &Path, profile: &str) -> (PathBuf, PathBuf) {
    let profile_dir = data_dir.join("wallets").join(profile);
    (
        profile_dir.join("credentials.bin"),
        profile_dir.join("salt.bin"),
    )
}

pub fn resolve_kdf_params(
    mem_kib_env: Option<&str>,
    iters_env: Option<&str>,
) -> anyhow::Result<argon2::Params> {
    let mem_kib = parse_kdf_param("HL_KDF_MEM_KiB", mem_kib_env, MIN_KDF_MEM_KIB)?;
    let iters = parse_kdf_param("HL_KDF_ITERS", iters_env, MIN_KDF_ITERS)?;
    argon2::Params::new(mem_kib, iters, 1, Some(32))
        .map_err(|err| anyhow::anyhow!("invalid Argon2 KDF parameters: {err}"))
}

fn derive_key(password: &[u8], salt: &[u8]) -> anyhow::Result<[u8; 32]> {
    let mem_env = env_kdf_var("HL_KDF_MEM_KiB")?;
    let iters_env = env_kdf_var("HL_KDF_ITERS")?;
    let params = resolve_kdf_params(mem_env.as_deref(), iters_env.as_deref())?;
    let argon2 = Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut out = [0_u8; 32];
    argon2.hash_password_into(password, salt, &mut out)?;
    Ok(out)
}

fn env_kdf_var(name: &str) -> anyhow::Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(err) => Err(anyhow::anyhow!("read {name}: {err}")),
    }
}

fn parse_kdf_param(name: &str, raw: Option<&str>, min_value: u32) -> anyhow::Result<u32> {
    match raw {
        None => Ok(min_value),
        Some(value) => {
            let parsed = value
                .trim()
                .parse::<u32>()
                .map_err(|_| anyhow::anyhow!("{name} must be an integer >= {min_value}"))?;
            anyhow::ensure!(
                parsed >= min_value,
                "{name} must be >= {min_value} (got {parsed})"
            );
            Ok(parsed)
        }
    }
}

fn upsert_profile_record(
    data_dir: &Path,
    profile: &str,
    credentials: &Credentials,
) -> anyhow::Result<()> {
    let path = data_dir.join("wallet_profiles.json");
    let mut index = if path.exists() {
        let index = serde_json::from_slice::<WalletProfilesIndex>(&fs::read(&path)?)?;
        validate_profile_index(&index)?;
        index
    } else {
        WalletProfilesIndex {
            version: 1,
            default_profile: profile.to_string(),
            profiles: Vec::new(),
        }
    };
    let now = now_ms();
    if let Some(existing) = index
        .profiles
        .iter_mut()
        .find(|record| record.id == profile)
    {
        existing.label = profile.to_string();
        existing.main_wallet = Some(credentials.main_wallet.clone());
        existing.network = Some(credentials.network.clone());
        existing.updated_at_ms = now;
    } else {
        index.profiles.push(WalletProfileRecord {
            id: profile.to_string(),
            label: profile.to_string(),
            main_wallet: Some(credentials.main_wallet.clone()),
            network: Some(credentials.network.clone()),
            created_at_ms: now,
            updated_at_ms: now,
        });
    }
    if index.default_profile.trim().is_empty() || index.profiles.len() == 1 {
        index.default_profile = profile.to_string();
    }
    write_secure_file(&path, &serde_json::to_vec_pretty(&index)?)?;
    Ok(())
}

fn validate_profile_index(index: &WalletProfilesIndex) -> anyhow::Result<()> {
    anyhow::ensure!(
        index.version == 1,
        "unsupported wallet profile index version"
    );
    let default = normalize_profile_id(&index.default_profile)?;
    let mut ids = std::collections::BTreeSet::new();
    for profile in &index.profiles {
        let id = normalize_profile_id(&profile.id)?;
        anyhow::ensure!(ids.insert(id), "duplicate wallet profile id {}", profile.id);
    }
    anyhow::ensure!(
        ids.contains(&default),
        "default wallet profile {default} is not present in profile index"
    );
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn secure_dir(path: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn write_secure_file(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        secure_dir(parent)?;
    }
    static WRITE_SEQ: AtomicU64 = AtomicU64::new(1);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("secure file path missing UTF-8 file name"))?;
    let temporary = path.with_file_name(format!(
        ".{file_name}.tmp-{}-{}",
        std::process::id(),
        WRITE_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> anyhow::Result<()> {
        let mut file = options.open(&temporary)?;
        file.write_all(data)?;
        file.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
        }
        fs::rename(&temporary, path)?;
        if let Some(parent) = path.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
