//! Epic account authentication.
//!
//! Two flows are supported:
//!
//! * **Device code** (default): `POST /account/api/oauth/deviceAuthorization` returns a code the
//!   user enters at `https://www.epicgames.com/activate`; we then poll `/account/api/oauth/token`
//!   with `grant_type=device_code`.
//! * **Authorization code**: for users who prefer a browser session (and therefore 2FA), the
//!   redirect payload's `authorizationCode` is exchanged with `grant_type=authorization_code`.
//!
//! Tokens are written to `%LOCALAPPDATA%\epicd\auth.json` protected with Windows DPAPI
//! (current-user scope). No password is ever handled by this program.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::http::{Client, Request};
use crate::net;
use crate::util;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

/// OAuth host used by the launcher client.
pub const OAUTH_HOST: &str = "https://account-public-service-prod.ol.epicgames.com";
const DEVICE_CODE_DEFAULT_TTL: u64 = 600;
const DEVICE_CODE_DEFAULT_INTERVAL: u64 = 10;
/// Refresh this many seconds before the access token actually expires.
const REFRESH_SKEW: u64 = 300;

/// A complete token set for one account.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenSet {
    pub account_id: String,
    #[serde(default)]
    pub display_name: String,
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub token_type: String,
    #[serde(default)]
    pub client_id: String,
    /// Unix timestamp (seconds) at which the access token expires.
    pub expires_at: u64,
    #[serde(default)]
    pub refresh_expires_at: u64,
    #[serde(default)]
    pub scope: String,
}

/// Decode base64url without padding. Returns `None` on any character outside the alphabet.
fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4 + 3);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for byte in input.bytes() {
        if byte == b'=' {
            break;
        }
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    // Leftover bits are the padding bits of a well-formed encoding; nothing to verify there.
    Some(out)
}

/// Read one string claim from the (unverified) JWT payload of an Epic access token.
///
/// The token is opaque to this program, but Epic embeds the account's display name in `dn`, which
/// is the only reliable source: the OAuth response for `launcherAppClient2` returns an empty
/// `display_name`, and a UI that falls back to the account id shows a bare 32-character hex blob.
fn jwt_claim(token: &str, key: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64url_decode(payload)?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value.get(key)?.as_str().map(str::to_string)
}

/// Percent-decode a UTF-8 string, or `None` when it is not valid percent-encoded UTF-8.
fn percent_decode_utf8(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = &text[i + 1..i + 3];
            let byte = u8::from_str_radix(hex, 16).ok()?;
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Map one character back to the single byte it stands for in Windows-1252.
///
/// Only used to undo a UTF-8→1252 mis-decode, where e.g. `测` (bytes `E6 B5 8B`) arrives as the
/// three characters `æµ‹`. Characters that have no single-byte form return `None`, which keeps a
/// genuine CJK/emoji name from being "repaired" into garbage.
fn cp1252_byte(c: char) -> Option<u8> {
    let code = c as u32;
    if code <= 0xff {
        return Some(code as u8);
    }
    let byte = match c {
        '\u{20ac}' => 0x80,
        '\u{201a}' => 0x82,
        '\u{0192}' => 0x83,
        '\u{201e}' => 0x84,
        '\u{2026}' => 0x85,
        '\u{2020}' => 0x86,
        '\u{2021}' => 0x87,
        '\u{02c6}' => 0x88,
        '\u{2030}' => 0x89,
        '\u{0160}' => 0x8a,
        '\u{2039}' => 0x8b,
        '\u{0152}' => 0x8c,
        '\u{017d}' => 0x8e,
        '\u{2018}' => 0x91,
        '\u{2019}' => 0x92,
        '\u{201c}' => 0x93,
        '\u{201d}' => 0x94,
        '\u{2022}' => 0x95,
        '\u{2013}' => 0x96,
        '\u{2014}' => 0x97,
        '\u{02dc}' => 0x98,
        '\u{2122}' => 0x99,
        '\u{0161}' => 0x9a,
        '\u{203a}' => 0x9b,
        '\u{0153}' => 0x9c,
        '\u{017e}' => 0x9e,
        '\u{0178}' => 0x9f,
        _ => return None,
    };
    Some(byte)
}

/// Undo UTF-8 text that was decoded as Windows-1252 somewhere upstream (`ä¸­æ–‡` → `中文`).
fn repair_mojibake(text: &str) -> Option<String> {
    if text.chars().all(|c| (c as u32) < 0x80) {
        return None;
    }
    let mut bytes = Vec::with_capacity(text.len());
    for c in text.chars() {
        bytes.push(cp1252_byte(c)?);
    }
    let repaired = String::from_utf8(bytes).ok()?;
    if repaired.is_empty() || repaired == text {
        None
    } else {
        Some(repaired)
    }
}

/// Normalise a display name coming from Epic: strip padding, percent-decode, repair mojibake.
fn normalize_display_name(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let decoded = if trimmed.contains('%') {
        percent_decode_utf8(trimmed).unwrap_or_else(|| trimmed.to_string())
    } else {
        trimmed.to_string()
    };
    repair_mojibake(&decoded).unwrap_or(decoded)
}

impl TokenSet {
    /// Best name to show for this account.
    ///
    /// Prefers the stored `display_name`; when Epic left it empty (the launcher client does), the
    /// JWT `dn` claim is used instead. Returns an empty string when neither source has a name, so a
    /// caller can show a neutral label rather than the raw account id.
    pub fn account_name(&self) -> String {
        let stored = normalize_display_name(&self.display_name);
        if !stored.is_empty() {
            return stored;
        }
        jwt_claim(&self.access_token, "dn")
            .map(|name| normalize_display_name(&name))
            .filter(|name| !name.is_empty())
            .unwrap_or_default()
    }

    /// Fill `display_name` from the access token when it is missing, so the value survives refreshes.
    fn backfill_display_name(&mut self) {
        if self.display_name.is_empty() {
            let name = self.account_name();
            if !name.is_empty() {
                self.display_name = name;
            }
        } else {
            self.display_name = normalize_display_name(&self.display_name);
        }
    }

    /// True when the access token is missing or about to expire.
    pub fn needs_refresh(&self) -> bool {
        let now = util::now_unix();
        self.access_token.is_empty() || self.expires_at <= now.saturating_add(REFRESH_SKEW)
    }

    /// True when the refresh token itself is expired.
    pub fn refresh_expired(&self) -> bool {
        self.refresh_expires_at != 0 && self.refresh_expires_at <= util::now_unix()
    }

    /// Authorization header value.
    pub fn bearer(&self) -> String {
        format!("bearer {}", self.access_token)
    }
}

#[derive(Debug, Deserialize)]
struct RawToken {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: u64,
    /// Epic sends `refresh_expires`; accept both spellings.
    #[serde(default, alias = "refresh_expires")]
    refresh_expires_in: u64,
    #[serde(default)]
    account_id: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    token_type: String,
    #[serde(default)]
    client_id: String,
    /// Epic sends an array of scope names here, not a string.
    #[serde(default)]
    scope: serde_json::Value,
}

fn scope_to_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join(","),
        _ => String::new(),
    }
}

impl RawToken {
    fn into_token_set(self) -> TokenSet {
        let now = util::now_unix();
        let mut set = TokenSet {
            account_id: self.account_id,
            display_name: self.display_name,
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            token_type: self.token_type,
            client_id: self.client_id,
            expires_at: now.saturating_add(self.expires_in.max(60)),
            refresh_expires_at: if self.refresh_expires_in > 0 {
                now.saturating_add(self.refresh_expires_in)
            } else {
                0
            },
            scope: scope_to_string(&self.scope),
        };
        // The launcher client's token response carries no `display_name`; the display name is inside
        // the access token itself, so recover it now instead of showing an account id forever.
        set.backfill_display_name();
        set
    }
}

/// Data shown to the user while a device-code login is pending.
#[derive(Debug, Clone)]
pub struct DevicePrompt {
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    pub expires_in: u64,
    pub interval: u64,
    pub device_code: String,
}

fn basic_form(req: Request, cfg: &Config) -> Request {
    req.basic_auth(&cfg.client_id, &cfg.client_secret)
}

/// Ask Epic for a device code.
pub fn request_device_code(cfg: &Config, client: &Client) -> Result<DevicePrompt> {
    let url = format!("{OAUTH_HOST}/account/api/oauth/deviceAuthorization");
    let req = basic_form(Request::post_form(&url, &[("client_id", cfg.client_id.as_str())]), cfg);
    let value = client.json_checked(req)?;

    let user_code = value
        .get("user_code")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Auth("device authorization response has no user_code".into()))?;
    let device_code = value
        .get("device_code")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Auth("device authorization response has no device_code".into()))?;

    Ok(DevicePrompt {
        user_code: user_code.to_string(),
        verification_uri: value
            .get("verification_uri")
            .and_then(|v| v.as_str())
            .unwrap_or("https://www.epicgames.com/activate")
            .to_string(),
        verification_uri_complete: value
            .get("verification_uri_complete")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        expires_in: value
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEVICE_CODE_DEFAULT_TTL),
        interval: value
            .get("interval")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEVICE_CODE_DEFAULT_INTERVAL)
            .max(1),
        device_code: device_code.to_string(),
    })
}

/// Poll until the user finishes the device-code login or the code expires.
///
/// `on_wait` is called before each poll round so a UI can render a countdown.
pub fn poll_device_code(
    cfg: &Config,
    client: &Client,
    prompt: &DevicePrompt,
    mut on_wait: impl FnMut(u64),
) -> Result<TokenSet> {
    let url = format!("{OAUTH_HOST}/account/api/oauth/token");
    let deadline = util::now_unix().saturating_add(prompt.expires_in);
    let mut interval = prompt.interval;

    loop {
        let now = util::now_unix();
        if now >= deadline {
            return Err(Error::Auth(
                "device code expired before the login was confirmed".into(),
            ));
        }
        on_wait(deadline - now);

        let req = basic_form(
            Request::post_form(
                &url,
                &[
                    ("grant_type", "device_code"),
                    ("device_code", prompt.device_code.as_str()),
                    ("token_type", "eg1"),
                ],
            ),
            cfg,
        );
        match client.json_checked(req) {
            Ok(value) => {
                let raw: RawToken = serde_json::from_value(value)
                    .map_err(|e| Error::Auth(format!("token response is malformed: {e}")))?;
                return Ok(raw.into_token_set());
            }
            Err(Error::Api { code, status, message }) => {
                if net::is_authorization_pending(&code) {
                    if code.ends_with("slow_down") {
                        interval = interval.saturating_add(5);
                    }
                    std::thread::sleep(Duration::from_secs(interval));
                    continue;
                }
                if code.ends_with("expired") {
                    return Err(Error::Auth("device code expired".into()));
                }
                return Err(Error::Api { status, code, message });
            }
            Err(other) => return Err(other),
        }
    }
}

/// Exchange a browser `authorizationCode` for tokens.
pub fn login_with_auth_code(cfg: &Config, client: &Client, code: &str) -> Result<TokenSet> {
    let url = format!("{OAUTH_HOST}/account/api/oauth/token");
    let req = basic_form(
        Request::post_form(
            &url,
            &[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("token_type", "eg1"),
            ],
        ),
        cfg,
    );
    let value = client.json_checked(req)?;
    let raw: RawToken = serde_json::from_value(value)
        .map_err(|e| Error::Auth(format!("token response is malformed: {e}")))?;
    Ok(raw.into_token_set())
}

/// Refresh an expired access token.
pub fn refresh(cfg: &Config, client: &Client, tokens: &TokenSet) -> Result<TokenSet> {
    let refresh_token = tokens
        .refresh_token
        .as_deref()
        .ok_or_else(|| Error::Auth("no refresh token stored; please log in again".into()))?;
    if tokens.refresh_expired() {
        return Err(Error::Auth("refresh token expired; please log in again".into()));
    }
    let url = format!("{OAUTH_HOST}/account/api/oauth/token");
    let req = basic_form(
        Request::post_form(
            &url,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("token_type", "eg1"),
            ],
        ),
        cfg,
    );
    let value = client.json_checked(req)?;
    let raw: RawToken = serde_json::from_value(value)
        .map_err(|e| Error::Auth(format!("refresh response is malformed: {e}")))?;
    let mut fresh = raw.into_token_set();
    if fresh.account_id.is_empty() {
        fresh.account_id = tokens.account_id.clone();
    }
    if fresh.display_name.is_empty() {
        fresh.display_name = tokens.display_name.clone();
    }
    Ok(fresh)
}

/// Return a token set that is valid right now, refreshing if necessary.
pub fn ensure_fresh(cfg: &Config, client: &Client, tokens: TokenSet) -> Result<TokenSet> {
    if tokens.needs_refresh() {
        let fresh = refresh(cfg, client, &tokens)?;
        save_tokens(&fresh)?;
        return Ok(fresh);
    }
    Ok(tokens)
}

/// Kill the remote session (best effort) and delete local tokens for one account.
pub fn logout(cfg: &Config, client: &Client, tokens: &TokenSet) -> Result<()> {
    let url = format!(
        "{OAUTH_HOST}/account/api/oauth/sessions/kill/{}",
        tokens.access_token
    );
    let mut req = Request::get(url).timeout(20).basic_auth(&cfg.client_id, &cfg.client_secret);
    req.method = crate::http::Method::Delete;
    // Body content is irrelevant; ignore transport failures on purpose.
    let mut sink = std::io::sink();
    let _ = client.request(&req, &mut sink);

    let mut store = StoreFile::load()?;
    store.accounts.remove(&tokens.account_id);
    if store.current.as_deref() == Some(tokens.account_id.as_str()) {
        store.current = store.accounts.keys().next().cloned();
    }
    store.save()
}

// ---------------------------------------------------------------------------------------------
// token persistence
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    #[serde(default)]
    current: Option<String>,
    #[serde(default)]
    accounts: BTreeMap<String, TokenSet>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Envelope {
    v: u32,
    enc: String,
    data: String,
}

fn store_path() -> PathBuf {
    Config::dir().join("auth.json")
}

impl StoreFile {
    fn load() -> Result<StoreFile> {
        let path = store_path();
        if !path.exists() {
            return Ok(StoreFile { version: 1, current: None, accounts: BTreeMap::new() });
        }
        let text = std::fs::read_to_string(&path)?;
        let envelope: Envelope = serde_json::from_str(&text)
            .map_err(|e| Error::Auth(format!("token store is corrupt: {e}")))?;
        let json = if envelope.enc == "dpapi" {
            let raw = hex::decode(&envelope.data)
                .map_err(|e| Error::Auth(format!("token store is corrupt: {e}")))?;
            let plain = dpapi_unprotect(&raw).ok_or_else(|| {
                Error::Auth(
                    "cannot decrypt the stored token file for this Windows user; run `epicd auth login` again"
                        .into(),
                )
            })?;
            String::from_utf8(plain)
                .map_err(|e| Error::Auth(format!("token store is corrupt: {e}")))?
        } else {
            envelope.data
        };
        let mut store: StoreFile = serde_json::from_str(&json)
            .map_err(|e| Error::Auth(format!("token store is corrupt: {e}")))?;
        if store.version == 0 {
            store.version = 1;
        }
        Ok(store)
    }

    fn save(&self) -> Result<()> {
        let json = serde_json::to_string(self)
            .map_err(|e| Error::Auth(format!("cannot serialize tokens: {e}")))?;
        let envelope = match dpapi_protect(json.as_bytes()) {
            Some(cipher) => Envelope { v: 1, enc: "dpapi".to_string(), data: hex::encode(cipher) },
            None => Envelope { v: 1, enc: "plain".to_string(), data: json },
        };
        let text = serde_json::to_string(&envelope)
            .map_err(|e| Error::Auth(format!("cannot serialize tokens: {e}")))?;
        util::atomic_write(&store_path(), text.as_bytes())
    }
}

/// Store (or replace) the tokens of an account and make it current.
pub fn save_tokens(tokens: &TokenSet) -> Result<()> {
    let mut store = StoreFile::load()?;
    store.version = 1;
    store.current = Some(tokens.account_id.clone());
    store.accounts.insert(tokens.account_id.clone(), tokens.clone());
    store.save()
}

/// Load the current account's tokens.
pub fn load_tokens() -> Result<Option<TokenSet>> {
    let store = StoreFile::load()?;
    let mut tokens = if let Some(current) = &store.current {
        store.accounts.get(current).cloned()
    } else {
        store.accounts.values().next().cloned()
    };
    // Token stores written before the `dn` fallback existed have an empty display name; repair them
    // in memory so every caller (CLI, GUI, web UI) shows the account name without a re-login.
    if let Some(tokens) = tokens.as_mut() {
        tokens.backfill_display_name();
    }
    Ok(tokens)
}

/// Load tokens for a specific account.
pub fn load_tokens_for(account_id: &str) -> Result<Option<TokenSet>> {
    let mut tokens = StoreFile::load()?.accounts.get(account_id).cloned();
    if let Some(tokens) = tokens.as_mut() {
        tokens.backfill_display_name();
    }
    Ok(tokens)
}

/// Known accounts as `(account_id, display_name)`.
pub fn accounts() -> Result<Vec<(String, String)>> {
    let store = StoreFile::load()?;
    Ok(store
        .accounts
        .iter()
        .map(|(id, t)| (id.clone(), t.account_name()))
        .collect())
}

/// Switch the active account.
pub fn set_current(account_id: &str) -> Result<()> {
    let mut store = StoreFile::load()?;
    if !store.accounts.contains_key(account_id) {
        return Err(Error::Auth(format!("unknown account {account_id}")));
    }
    store.current = Some(account_id.to_string());
    store.save()
}


/// Verify the token store can be written *before* a one-time authorization code is consumed.
///
/// Losing a code because the state directory is not writable is a bad failure mode: the code is
/// single-use and short lived, so the check has to happen first.
pub fn ensure_store_writable() -> Result<()> {
    let dir = Config::dir();
    std::fs::create_dir_all(&dir).map_err(|e| {
        Error::Auth(format!("cannot create state directory {}: {e}", dir.display()))
    })?;
    let probe = dir.join(".write-probe");
    std::fs::write(&probe, b"ok").map_err(|e| {
        Error::Auth(format!(
            "cannot write into state directory {}: {e} (set EPICD_HOME to a writable folder)",
            dir.display()
        ))
    })?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// Absolute path of the token store, for diagnostics.
pub fn store_path_display() -> String {
    store_path().display().to_string()
}
/// Remove every stored token.
pub fn clear_tokens() -> Result<()> {
    let path = store_path();
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Windows DPAPI
// ---------------------------------------------------------------------------------------------

#[cfg(windows)]
mod dpapi_impl {
    use std::ffi::c_void;

    #[repr(C)]
    struct Blob {
        cb: u32,
        pb: *mut u8,
    }

    #[link(name = "crypt32")]
    extern "system" {
        fn CryptProtectData(
            data_in: *const Blob,
            desc: *const u16,
            entropy: *const Blob,
            reserved: *mut c_void,
            prompt: *mut c_void,
            flags: u32,
            data_out: *mut Blob,
        ) -> i32;
        fn CryptUnprotectData(
            data_in: *const Blob,
            desc: *mut *mut u16,
            entropy: *const Blob,
            reserved: *mut c_void,
            prompt: *mut c_void,
            flags: u32,
            data_out: *mut Blob,
        ) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn LocalFree(mem: *mut c_void) -> *mut c_void;
    }

    const CRYPTPROTECT_UI_FORBIDDEN: u32 = 0x1;

    pub fn protect(data: &[u8]) -> Option<Vec<u8>> {
        let input = Blob { cb: data.len() as u32, pb: data.as_ptr() as *mut u8 };
        let mut output = Blob { cb: 0, pb: std::ptr::null_mut() };
        let ok = unsafe {
            CryptProtectData(
                &input,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        if ok == 0 {
            return None;
        }
        let out = unsafe { std::slice::from_raw_parts(output.pb, output.cb as usize).to_vec() };
        unsafe {
            LocalFree(output.pb as *mut c_void);
        }
        Some(out)
    }

    pub fn unprotect(data: &[u8]) -> Option<Vec<u8>> {
        let input = Blob { cb: data.len() as u32, pb: data.as_ptr() as *mut u8 };
        let mut output = Blob { cb: 0, pb: std::ptr::null_mut() };
        let ok = unsafe {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        if ok == 0 {
            return None;
        }
        let out = unsafe { std::slice::from_raw_parts(output.pb, output.cb as usize).to_vec() };
        unsafe {
            LocalFree(output.pb as *mut c_void);
        }
        Some(out)
    }
}

#[cfg(windows)]
fn dpapi_protect(data: &[u8]) -> Option<Vec<u8>> {
    dpapi_impl::protect(data)
}

#[cfg(windows)]
fn dpapi_unprotect(data: &[u8]) -> Option<Vec<u8>> {
    dpapi_impl::unprotect(data)
}

#[cfg(not(windows))]
fn dpapi_protect(_data: &[u8]) -> Option<Vec<u8>> {
    None
}

#[cfg(not(windows))]
fn dpapi_unprotect(_data: &[u8]) -> Option<Vec<u8>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(account: &str, expires_in: i64) -> TokenSet {
        TokenSet {
            account_id: account.to_string(),
            display_name: "Tester".to_string(),
            access_token: "access".to_string(),
            refresh_token: Some("refresh".to_string()),
            token_type: "bearer".to_string(),
            client_id: "client".to_string(),
            expires_at: (util::now_unix() as i64 + expires_in).max(0) as u64,
            refresh_expires_at: (util::now_unix() as i64 + 86_400).max(0) as u64,
            scope: String::new(),
        }
    }

    #[test]
    fn detects_expiring_tokens() {
        assert!(token("a", 10).needs_refresh());
        assert!(!token("a", 3600).needs_refresh());
    }

    #[test]
    fn bearer_header_shape() {
        assert_eq!(token("a", 3600).bearer(), "bearer access");
    }

    #[test]
    fn dpapi_roundtrip_when_available() {
        let data = b"epicd token payload";
        match dpapi_protect(data) {
            Some(cipher) => {
                assert_ne!(cipher.as_slice(), data);
                let plain = dpapi_unprotect(&cipher).expect("unprotect");
                assert_eq!(plain, data);
            }
            None => eprintln!("DPAPI unavailable, skipping roundtrip assertion"),
        }
    }

    #[test]
    fn store_file_roundtrips_via_envelope() {
        let mut store =
            StoreFile { version: 1, current: Some("acc".into()), accounts: BTreeMap::new() };
        store.accounts.insert("acc".into(), token("acc", 3600));
        let json = serde_json::to_string(&store).unwrap();
        let envelope = match dpapi_protect(json.as_bytes()) {
            Some(cipher) => Envelope { v: 1, enc: "dpapi".into(), data: hex::encode(cipher) },
            None => Envelope { v: 1, enc: "plain".into(), data: json.clone() },
        };
        let text = serde_json::to_string(&envelope).unwrap();
        let back: Envelope = serde_json::from_str(&text).unwrap();
        let decoded = if back.enc == "dpapi" {
            String::from_utf8(dpapi_unprotect(&hex::decode(back.data).unwrap()).unwrap()).unwrap()
        } else {
            back.data
        };
        let store2: StoreFile = serde_json::from_str(&decoded).unwrap();
        assert_eq!(store2.current.as_deref(), Some("acc"));
        assert_eq!(store2.accounts.len(), 1);
    }

    /// `{"dn":"测试用户","sub":"af83ec28…"}` as a base64url JWT segment.
    const DN_SEGMENT: &str = "eyJkbiI6Iua1i-ivleeUqOaItyIsInN1YiI6ImFmODNlYzI4MmM2ZjRiMThhYmE1YmY3MTc1ZDZiZTY2In0";
    /// `{"dn":"%E6%B5%8B%E8%AF%95","sub":"x"}` (percent-encoded name).
    const DN_ENCODED_SEGMENT: &str =
        "eyJkbiI6IiVFNiVCNSU4QiVFOCVBRiU5NSIsInN1YiI6IngifQ";
    /// `{"dn":"æµ‹è¯•","sub":"x"}` (UTF-8 read back as Windows-1252).
    const DN_MOJIBAKE_SEGMENT: &str = "eyJkbiI6IsOmwrXigLnDqMKv4oCiIiwic3ViIjoieCJ9";

    fn access_token(segment: &str) -> String {
        format!("eg1~header.{segment}.signature")
    }

    #[test]
    fn recovers_display_name_from_access_token() {
        let mut t = token("acc", 3600);
        t.display_name.clear();
        t.access_token = access_token(DN_SEGMENT);
        assert_eq!(t.account_name(), "测试用户");
    }

    #[test]
    fn decodes_percent_encoded_display_name() {
        let mut t = token("acc", 3600);
        t.display_name.clear();
        t.access_token = access_token(DN_ENCODED_SEGMENT);
        assert_eq!(t.account_name(), "测试");
    }

    #[test]
    fn repairs_mojibake_display_name() {
        let mut t = token("acc", 3600);
        t.display_name.clear();
        t.access_token = access_token(DN_MOJIBAKE_SEGMENT);
        assert_eq!(t.account_name(), "测试");
    }

    #[test]
    fn stored_display_name_wins_over_token_claim() {
        let mut t = token("acc", 3600);
        t.display_name = "Stored".to_string();
        t.access_token = access_token(DN_SEGMENT);
        assert_eq!(t.account_name(), "Stored");
    }

    #[test]
    fn missing_name_is_empty_not_the_account_id() {
        let mut t = token("acc", 3600);
        t.display_name.clear();
        t.access_token = "opaque-token".to_string();
        assert_eq!(t.account_name(), "");
    }

    #[test]
    fn latin_names_are_not_mistaken_for_mojibake() {
        assert_eq!(normalize_display_name("  Caf\u{e9}  "), "Caf\u{e9}");
        assert_eq!(normalize_display_name("J\u{f8}rgen"), "J\u{f8}rgen");
    }

    #[test]
    fn base64url_roundtrip_of_known_payload() {
        let bytes = base64url_decode(DN_SEGMENT).expect("decode");
        let text = String::from_utf8(bytes).expect("utf8");
        assert!(text.contains("\"dn\":\"测试用户\""));
        assert!(base64url_decode("no spaces allowed").is_none());
    }
}
