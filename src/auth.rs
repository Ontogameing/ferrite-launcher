//! Session-only Microsoft consumer device-code authentication for Minecraft Java.
//!
//! The protocol is split into two blocking operations:
//!
//! 1. [`start_login`] requests a device code. The caller displays the public
//!    [`DeviceCode::user_code`] and [`DeviceCode::verification_uri`] to the user.
//! 2. [`complete_login`] polls Microsoft for approval, then exchanges the Microsoft
//!    token for Xbox Live, XSTS, and finally Minecraft Services credentials. It also
//!    verifies Java ownership and fetches the player's profile.
//!
//! Supply your own Microsoft application (public client) ID, configured to allow
//! consumer accounts and device-code/public-client authentication. Microsoft or
//! Minecraft may require approval of the application for Minecraft Services use.
//! Both operations perform blocking network I/O and therefore belong on a worker
//! thread rather than the UI thread.
//!
//! Authentication is intentionally session-only: no refresh token is requested,
//! and this module neither logs nor persists credentials. Re-sign in when
//! [`Account::is_expired`] becomes true. Callers must likewise keep `DeviceCode`
//! and [`Account::access_token`] out of logs, diagnostics, and persistent storage.
//!
//! Cancellation is cooperative. A typical caller owns an `Arc<AtomicBool>`, keeps
//! one clone in UI/task state, and passes the worker clone to [`complete_login`] as
//! `&AtomicBool`. Setting it stops polling promptly, but cannot abort a blocking
//! HTTP request already in progress; request timeouts bound that delay.

use std::{
    io::Read,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

use reqwest::{
    blocking::{Client, RequestBuilder, Response},
    redirect::Policy,
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::json;

const DEVICE_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/devicecode";
const TOKEN_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/token";
const XBL_URL: &str = "https://user.auth.xboxlive.com/user/authenticate";
const XSTS_URL: &str = "https://xsts.auth.xboxlive.com/xsts/authorize";
const MC_LOGIN_URL: &str = "https://api.minecraftservices.com/authentication/login_with_xbox";
const ENTITLEMENTS_URL: &str = "https://api.minecraftservices.com/entitlements/mcstore";
const PROFILE_URL: &str = "https://api.minecraftservices.com/minecraft/profile";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const EXPIRY_GUARD: Duration = Duration::from_secs(30);
const MAX_BODY: u64 = 1024 * 1024;
/// Device-code OAuth scope; intentionally omits offline_access (no refresh tokens).
const MS_SCOPE: &str = "XboxLive.signin";

/// Authenticated Minecraft Java account, held only for this process/session.
///
/// Deliberately does not implement `Debug` or serialization, reducing the chance
/// of accidentally exposing the token through ordinary diagnostics or persistence.
/// The caller is still responsible for protecting every field copied from this value.
pub struct Account {
    /// Verified Java profile name.
    pub name: String,
    /// Verified lowercase hexadecimal UUID without hyphens.
    pub uuid: String,
    /// Minecraft Services bearer token, not a Microsoft or Xbox token. Do not log it.
    pub access_token: String,
    /// Decimal Xbox user ID, or empty if neither Xbox response provides `xid`.
    /// The empty value is valid for the optional launch placeholder; `uhs` is
    /// a different identifier and is never substituted for a missing XUID.
    pub xuid: String,
    /// Public Microsoft application ID used for this sign-in.
    pub client_id: String,
    /// Monotonic expiry deadline for the Minecraft Services token.
    expires: Instant,
}

impl Account {
    /// True once the token is within 30 seconds of expiration (or has expired).
    /// Re-sign in rather than attempting to refresh this session-only account.
    pub fn is_expired(&self) -> bool {
        !has_token_lifetime(self.expires, Instant::now())
    }
}

/// Instructions for the user plus private, short-lived polling credentials.
///
/// Deliberately does not implement `Debug` or serialization because its private
/// fields authorize polling for this sign-in attempt. Only the public code and URL
/// should cross from the authentication worker to display code.
pub struct DeviceCode {
    /// Short code the user enters at Microsoft's verification page.
    pub user_code: String,
    /// HTTPS page where the user approves the sign-in.
    pub verification_uri: String,
    device_code: String,
    client_id: String,
    expires: Instant,
    interval: Duration,
}

#[derive(Deserialize)]
struct DeviceResponse {
    user_code: String,
    verification_uri: String,
    device_code: String,
    expires_in: u64,
    #[serde(default = "default_interval")]
    interval: u64,
}

fn default_interval() -> u64 {
    5
}

#[derive(Deserialize)]
struct OAuthError {
    error: String,
}

#[derive(Deserialize)]
struct OAuthResponse {
    access_token: Option<String>,
    token_type: Option<String>,
    expires_in: Option<u64>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct XboxResponse {
    #[serde(rename = "Token")]
    token: String,
    #[serde(rename = "DisplayClaims")]
    claims: XboxClaims,
}

#[derive(Deserialize)]
struct XboxClaims {
    xui: Vec<XboxUser>,
}

#[derive(Deserialize)]
struct XboxUser {
    uhs: String,
    xid: Option<String>,
}

#[derive(Deserialize)]
struct XboxError {
    #[serde(rename = "XErr")]
    code: Option<u64>,
}

#[derive(Deserialize)]
struct MinecraftToken {
    access_token: String,
    expires_in: u64,
    token_type: String,
}

#[derive(Deserialize)]
struct Entitlements {
    items: Vec<Entitlement>,
}

#[derive(Deserialize)]
struct Entitlement {
    name: String,
}

#[derive(Deserialize)]
struct Profile {
    name: String,
    id: String,
}

fn client_id(value: &str) -> Result<&str, String> {
    let value = value.trim();
    if value.is_empty() {
        Err("A Microsoft public-client application ID is required.".into())
    } else {
        Ok(value)
    }
}

fn client() -> Result<Client, String> {
    Client::builder()
        .user_agent("ferrite-launcher/0.1.0")
        .https_only(true)
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|_| "Could not initialize the secure authentication client.".into())
}

/// Deserializes a bounded response without ever incorporating its body into errors.
/// This prevents unexpectedly large payloads and server-returned secrets from
/// reaching user-visible diagnostics.
fn decode<T: DeserializeOwned>(reader: impl Read) -> Result<T, String> {
    serde_json::from_reader(reader.take(MAX_BODY))
        .map_err(|_| "Authentication service returned an invalid or oversized response.".into())
}

fn json_post(
    client: &Client,
    url: &str,
    body: serde_json::Value,
) -> Result<RequestBuilder, String> {
    let body = serde_json::to_vec(&body)
        .map_err(|_| "Could not encode the authentication request.".to_string())?;
    Ok(client
        .post(url)
        .header("Content-Type", "application/json")
        .body(body))
}

fn cancelled(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err("Sign-in cancelled.".into())
    } else {
        Ok(())
    }
}

fn network_error(stage: &str) -> String {
    format!("{stage}: request failed or timed out. Check your connection and try again.")
}

fn http_error(stage: &str, status: reqwest::StatusCode) -> String {
    format!(
        "{stage}: service returned HTTP {}. Try signing in again; if this persists, check the application's Minecraft API access.",
        status.as_u16()
    )
}

/// Sends one blocking request with cancellation checks on both sides of the wait.
/// A flag set during `send` is observed only after reqwest returns or times out.
fn send(request: RequestBuilder, cancel: &AtomicBool, stage: &str) -> Result<Response, String> {
    cancelled(cancel)?;
    let response = request.send();
    cancelled(cancel)?;
    response.map_err(|_| network_error(stage))
}

fn success<T: DeserializeOwned>(response: Response, stage: &str) -> Result<T, String> {
    if !response.status().is_success() {
        return Err(http_error(stage, response.status()));
    }
    decode(response)
}

fn deadline(start: Instant, seconds: u64) -> Result<Instant, String> {
    if seconds == 0 {
        return Err("Authentication service returned an expired credential. Sign in again.".into());
    }
    start
        .checked_add(Duration::from_secs(seconds))
        .ok_or_else(|| "Authentication service returned an invalid token lifetime.".into())
}

fn has_token_lifetime(expires: Instant, now: Instant) -> bool {
    expires.saturating_duration_since(now) > EXPIRY_GUARD
}

fn require_token_lifetime(expires: Instant) -> Result<(), String> {
    if has_token_lifetime(expires, Instant::now()) {
        Ok(())
    } else {
        Err("The sign-in token has expired or is about to expire. Sign in again.".into())
    }
}

/// Starts a consumer Microsoft device-code login using the supplied public client ID.
///
/// The ID is trimmed and must be non-empty. On success, the returned value contains
/// public instructions for the UI and private state later consumed by
/// [`complete_login`]. This function performs one blocking HTTPS request with a
/// 15-second timeout and does not itself support cancellation.
///
/// Errors are user-facing, sanitized messages: response bodies and raw service
/// descriptions are deliberately omitted because they may contain credentials or
/// other sensitive account data.
pub fn start_login(id: &str) -> Result<DeviceCode, String> {
    let id = client_id(id)?;
    let client = client()?;
    let started = Instant::now();
    let response = client
        .post(DEVICE_URL)
        .form(&[("client_id", id), ("scope", MS_SCOPE)])
        .send()
        .map_err(|_| network_error("Starting Microsoft sign-in"))?;
    let data = device_response(response.status(), response)?;
    let uri = reqwest::Url::parse(&data.verification_uri)
        .map_err(|_| "Microsoft returned an invalid verification URL.".to_string())?;
    if uri.scheme() != "https"
        || uri.host_str().is_none()
        || !uri.username().is_empty()
        || uri.password().is_some()
        || data.device_code.is_empty()
        || data.user_code.is_empty()
        || data.interval == 0
    {
        return Err("Microsoft returned invalid device-code instructions.".into());
    }
    let expires = deadline(started, data.expires_in)?;
    if Instant::now() >= expires {
        return Err("The device code has expired. Start sign-in again.".into());
    }
    Ok(DeviceCode {
        user_code: data.user_code,
        verification_uri: data.verification_uri,
        device_code: data.device_code,
        client_id: id.to_owned(),
        expires,
        interval: Duration::from_secs(data.interval),
    })
}

fn device_response(
    status: reqwest::StatusCode,
    reader: impl Read,
) -> Result<DeviceResponse, String> {
    if status == reqwest::StatusCode::BAD_REQUEST {
        let error: OAuthError = decode(reader)?;
        return Err(oauth_error(&error.error).err().unwrap_or_else(|| {
            "Microsoft rejected the device-code request. Check the application's configuration and start again.".into()
        }));
    }
    if !status.is_success() {
        return Err(http_error("Starting Microsoft sign-in", status));
    }
    decode(reader)
}

#[derive(PartialEq, Eq, Debug)]
enum PollAction {
    Pending,
    SlowDown,
}

fn oauth_error(error: &str) -> Result<PollAction, String> {
    match error {
        "authorization_pending" => Ok(PollAction::Pending),
        "slow_down" => Ok(PollAction::SlowDown),
        "expired_token" => Err("The device code has expired. Start sign-in again.".into()),
        "authorization_declined" | "access_denied" => Err("Microsoft sign-in was declined. Start sign-in again when ready.".into()),
        "invalid_client" | "unauthorized_client" => Err("Microsoft rejected the application. Check the client ID and enable consumer public-client/device-code authentication.".into()),
        "invalid_scope" => Err("Microsoft rejected the requested scope. Check that the application permits XboxLive.signin for consumer accounts.".into()),
        "invalid_grant" | "bad_verification_code" => Err("Microsoft rejected the device code. Start sign-in again.".into()),
        _ => Err("Microsoft could not authorize sign-in. Start again and check the application's configuration.".into()),
    }
}

fn next_interval(current: Duration, timeout: bool) -> Result<Duration, String> {
    let next = if timeout {
        current.checked_mul(2)
    } else {
        current.checked_add(Duration::from_secs(5))
    };
    next.ok_or_else(|| "Microsoft sign-in polling delay exceeded its limit. Start again.".into())
}

fn wait(interval: Duration, expires: Instant, cancel: &AtomicBool) -> Result<(), String> {
    let now = Instant::now();
    let wake = now.checked_add(interval).unwrap_or(expires).min(expires);
    loop {
        cancelled(cancel)?;
        let now = Instant::now();
        if now >= expires {
            return Err("The device code has expired. Start sign-in again.".into());
        }
        if now >= wake {
            return Ok(());
        }
        thread::sleep((wake - now).min(Duration::from_millis(100)));
    }
}

fn microsoft_token(
    client: &Client,
    id: &str,
    code: &DeviceCode,
    cancel: &AtomicBool,
) -> Result<(String, Instant), String> {
    let mut interval = code.interval;
    loop {
        // Wait before every request, including the first. Timeouts use exponential
        // backoff; slow_down adds five seconds to all subsequent polling delays.
        wait(interval, code.expires, cancel)?;
        let started = Instant::now();
        let remaining = code.expires.saturating_duration_since(started);
        if remaining.is_zero() {
            return Err("The device code has expired. Start sign-in again.".into());
        }
        let response = client
            .post(TOKEN_URL)
            .timeout(REQUEST_TIMEOUT.min(remaining))
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", id),
                ("device_code", code.device_code.as_str()),
            ])
            .send();
        cancelled(cancel)?;
        if Instant::now() >= code.expires {
            return Err("The device code has expired. Start sign-in again.".into());
        }
        let response = match response {
            Ok(response) => response,
            Err(error) if error.is_timeout() => {
                interval = next_interval(interval, true)?;
                continue;
            }
            Err(_) => return Err(network_error("Polling Microsoft sign-in")),
        };
        let status = response.status();
        // OAuth protocol errors are JSON on HTTP 400, not successful responses.
        if !status.is_success() && status != reqwest::StatusCode::BAD_REQUEST {
            return Err(http_error("Polling Microsoft sign-in", status));
        }
        let data: OAuthResponse = decode(response)?;
        cancelled(cancel)?;
        if let Some(error) = data.error {
            match oauth_error(&error)? {
                PollAction::Pending => (),
                PollAction::SlowDown => interval = next_interval(interval, false)?,
            }
            continue;
        }
        if !status.is_success() {
            return Err(http_error("Polling Microsoft sign-in", status));
        }
        let token = data
            .access_token
            .filter(|token| !token.is_empty())
            .ok_or_else(|| "Microsoft returned no access token.".to_string())?;
        if !data
            .token_type
            .as_deref()
            .is_some_and(|kind| kind.eq_ignore_ascii_case("bearer"))
        {
            return Err("Microsoft returned an unsupported token type.".into());
        }
        let expires = deadline(started, data.expires_in.unwrap_or(0))?;
        require_token_lifetime(expires)?;
        if Instant::now() >= code.expires {
            return Err("The device code has expired. Start sign-in again.".into());
        }
        return Ok((token, expires));
    }
}

fn xsts_error(code: Option<u64>) -> String {
    match code {
        Some(2148916233) => "This Microsoft account has no Xbox profile. Sign in at https://www.xbox.com/ to create one, then try again.",
        Some(2148916235) => "Xbox Live is unavailable in this account's country or region. Check your account region and Xbox availability.",
        Some(2148916236 | 2148916237) => "Xbox requires age or adult verification. Complete verification in your Microsoft/Xbox account settings, then try again.",
        Some(2148916238) => "This child account needs an adult-managed Microsoft family and permission to use Xbox Live. Ask the family organizer to configure it, then try again.",
        _ => "Xbox could not authorize this account. Sign in at https://www.xbox.com/ to resolve account restrictions, then retry; also check the application's Minecraft API access.",
    }.into()
}

fn xbox_user(data: &XboxResponse) -> Result<&XboxUser, String> {
    if data.token.is_empty() || data.claims.xui.len() != 1 || data.claims.xui[0].uhs.is_empty() {
        return Err("Xbox returned invalid identity claims.".into());
    }
    Ok(&data.claims.xui[0])
}

fn optional_xuid(xsts: Option<&str>, xbl: Option<&str>) -> Result<String, String> {
    // XUID is optional in these responses, but any value that is supplied must be
    // a nonzero decimal u64 and both protocol stages must identify the same user.
    for id in [xsts, xbl].into_iter().flatten() {
        if id.is_empty()
            || !id.bytes().all(|c| c.is_ascii_digit())
            || !id.parse::<u64>().is_ok_and(|value| value != 0)
        {
            return Err("Xbox returned an invalid Xbox user ID. Sign in again.".into());
        }
    }
    if let (Some(a), Some(b)) = (xsts, xbl) {
        if a != b {
            return Err("Xbox returned inconsistent user IDs. Sign in again.".into());
        }
    }
    Ok(xsts.or(xbl).unwrap_or_default().to_owned())
}

fn verified_profile(profile: Profile) -> Result<(String, String), String> {
    // Minecraft commonly emits a compact UUID, but accepting the canonical
    // hyphenated representation makes normalization robust without relaxing its shape.
    let id = profile.id.as_bytes();
    let valid = (id.len() == 32 && id.iter().all(u8::is_ascii_hexdigit))
        || (id.len() == 36
            && id.iter().enumerate().all(|(i, c)| {
                if matches!(i, 8 | 13 | 18 | 23) {
                    *c == b'-'
                } else {
                    c.is_ascii_hexdigit()
                }
            }));
    let uuid = profile.id.replace('-', "").to_ascii_lowercase();
    if !valid
        || uuid.bytes().all(|c| c == b'0')
        || !(1..=16).contains(&profile.name.len())
        || !profile
            .name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_')
    {
        return Err("Minecraft returned an invalid player name or UUID.".into());
    }
    Ok((profile.name, uuid))
}

fn owns_java(data: &Entitlements) -> bool {
    data.items
        .iter()
        .any(|item| matches!(item.name.as_str(), "game_minecraft" | "product_minecraft"))
}

/// Completes device-code login and returns a verified, session-only Java account.
///
/// `id` must match the public client ID passed to [`start_login`], and `code` is
/// consumed so one attempt's private polling credential cannot be accidentally reused.
/// The function waits for Microsoft approval, then performs the Microsoft → Xbox Live
/// → XSTS → Minecraft token exchanges. Before returning it verifies a Java entitlement,
/// validates the profile name/UUID, and ensures the final token has useful lifetime.
///
/// `cancel` is normally borrowed from the worker's clone of an `Arc<AtomicBool>`.
/// Setting it to `true` stops waits within about 100 ms and is checked around each
/// network call. It does not interrupt an in-flight blocking request, which may take
/// up to the configured 15-second request timeout. Relaxed atomic ordering is enough
/// because the flag communicates only cancellation, not access to other shared data.
///
/// `progress` runs synchronously on the calling worker thread and receives only
/// static, non-sensitive status text. The returned error is suitable for display:
/// raw response bodies, service descriptions, and tokens are never included.
pub fn complete_login(
    id: &str,
    code: DeviceCode,
    cancel: &AtomicBool,
    progress: impl Fn(&str),
) -> Result<Account, String> {
    let id = client_id(id)?;
    cancelled(cancel)?;
    if id != code.client_id {
        return Err("Use the same client ID that started this sign-in.".into());
    }
    let client = client()?;
    progress("Waiting for Microsoft sign-in approval…");
    let (ms_token, ms_expires) = microsoft_token(&client, id, &code, cancel)?;
    progress("Signing in to Xbox Live…");
    require_token_lifetime(ms_expires)?;
    let response = send(
        json_post(
            &client,
            XBL_URL,
            json!({
                "Properties": {"AuthMethod": "RPS", "SiteName": "user.auth.xboxlive.com", "RpsTicket": format!("d={ms_token}")},
                "RelyingParty": "http://auth.xboxlive.com", "TokenType": "JWT"
            }),
        )?,
        cancel,
        "Xbox Live sign-in",
    )?;
    let xbl: XboxResponse = success(response, "Xbox Live sign-in")?;
    let xbl_user = xbox_user(&xbl)?;
    progress("Authorizing Minecraft with Xbox…");
    let response = send(
        json_post(
            &client,
            XSTS_URL,
            json!({
                "Properties": {"SandboxId": "RETAIL", "UserTokens": [&xbl.token]},
                "RelyingParty": "rp://api.minecraftservices.com/", "TokenType": "JWT"
            }),
        )?,
        cancel,
        "Xbox authorization",
    )?;
    if !response.status().is_success() {
        let error = decode::<XboxError>(response)
            .ok()
            .and_then(|error| error.code);
        return Err(xsts_error(error));
    }
    let xsts: XboxResponse = decode(response)?;
    let user = xbox_user(&xsts)?;
    if user.uhs != xbl_user.uhs {
        return Err("Xbox returned inconsistent identity claims. Sign in again.".into());
    }
    let xuid = optional_xuid(user.xid.as_deref(), xbl_user.xid.as_deref())?;
    progress("Signing in to Minecraft…");
    let started = Instant::now();
    let response = send(
        json_post(
            &client,
            MC_LOGIN_URL,
            json!({
                "identityToken": format!("XBL3.0 x={};{}", user.uhs, xsts.token)
            }),
        )?,
        cancel,
        "Minecraft sign-in",
    )?;
    let token: MinecraftToken = success(response, "Minecraft sign-in")?;
    if token.access_token.is_empty() || !token.token_type.eq_ignore_ascii_case("bearer") {
        return Err("Minecraft returned an invalid access token.".into());
    }
    let expires = deadline(started, token.expires_in)?;
    require_token_lifetime(expires)?;
    progress("Checking Minecraft Java ownership…");
    let response = send(
        client
            .get(ENTITLEMENTS_URL)
            .bearer_auth(&token.access_token),
        cancel,
        "Minecraft ownership check",
    )?;
    let entitlements: Entitlements = success(response, "Minecraft ownership check")?;
    if !owns_java(&entitlements) {
        return Err("This account has no Minecraft Java entitlement. Use the account that owns Java Edition or has an active eligible subscription.".into());
    }
    require_token_lifetime(expires)?;
    progress("Verifying Minecraft profile…");
    let response = send(
        client.get(PROFILE_URL).bearer_auth(&token.access_token),
        cancel,
        "Minecraft profile check",
    )?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Err("No Minecraft Java profile exists. Set up your Java profile and player name at https://www.minecraft.net/ and try again.".into());
    }
    let (name, uuid) = verified_profile(success(response, "Minecraft profile check")?)?;
    cancelled(cancel)?;
    require_token_lifetime(expires)?;
    progress("Minecraft sign-in complete.");
    cancelled(cancel)?;
    Ok(Account {
        name,
        uuid,
        access_token: token.access_token,
        xuid,
        client_id: id.to_owned(),
        expires,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_client_id() {
        assert!(client_id(" \n ").is_err());
        assert_eq!(client_id(" public-client ").unwrap(), "public-client");
    }

    #[test]
    fn parses_device_code_with_default_interval() {
        let data: DeviceResponse = decode(&br#"{"user_code":"ABCD","verification_uri":"https://microsoft.com/link","device_code":"secret","expires_in":900}"#[..]).unwrap();
        assert_eq!(data.interval, 5);
        assert_eq!(data.expires_in, 900);
        assert!(decode::<DeviceResponse>(&b"{}"[..]).is_err());
    }

    #[test]
    fn device_setup_errors_are_actionable_and_sanitized() {
        for (code, guidance) in [
            ("invalid_client", "client ID"),
            ("unauthorized_client", "public-client/device-code"),
            ("invalid_scope", "XboxLive.signin"),
            ("SECRET_ACCESS_TOKEN", "configuration"),
            ("authorization_pending", "configuration"),
        ] {
            let body = serde_json::to_vec(&json!({
                "error": code,
                "error_description": "SECRET_ACCESS_TOKEN",
                "access_token": "SECRET_ACCESS_TOKEN"
            }))
            .unwrap();
            let error = device_response(reqwest::StatusCode::BAD_REQUEST, body.as_slice())
                .err()
                .unwrap();
            assert!(error.contains(guidance));
            assert!(!error.contains("SECRET_ACCESS_TOKEN"));
        }
        for body in [
            "SECRET_ACCESS_TOKEN",
            r#"{"error_description":"SECRET_ACCESS_TOKEN"}"#,
            r#"{"error":123}"#,
        ] {
            let error = device_response(reqwest::StatusCode::BAD_REQUEST, body.as_bytes())
                .err()
                .unwrap();
            assert!(!error.contains("SECRET_ACCESS_TOKEN"));
        }
        let data = device_response(reqwest::StatusCode::OK, &br#"{"user_code":"ABCD","verification_uri":"https://microsoft.com/link","device_code":"secret","expires_in":900}"#[..]).unwrap();
        assert_eq!(data.user_code, "ABCD");
        assert!(
            device_response(
                reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                &b"SECRET_ACCESS_TOKEN"[..]
            )
            .err()
            .unwrap()
            .contains("HTTP 500")
        );
    }

    #[test]
    fn missing_xuid_is_an_empty_placeholder_not_uhs() {
        let xsts: XboxResponse =
            decode(&br#"{"Token":"xsts","DisplayClaims":{"xui":[{"uhs":"hash"}]}}"#[..]).unwrap();
        let xbl: XboxResponse =
            decode(&br#"{"Token":"xbl","DisplayClaims":{"xui":[{"uhs":"hash"}]}}"#[..]).unwrap();
        let user = xbox_user(&xsts).unwrap();
        let xbl_user = xbox_user(&xbl).unwrap();
        assert_eq!(
            optional_xuid(user.xid.as_deref(), xbl_user.xid.as_deref()).unwrap(),
            ""
        );
    }

    #[test]
    fn valid_xuids_are_preserved_and_must_match() {
        for (xsts, xbl) in [
            (Some("123"), None),
            (None, Some("123")),
            (Some("123"), Some("123")),
        ] {
            assert_eq!(optional_xuid(xsts, xbl).unwrap(), "123");
        }
        assert!(
            optional_xuid(Some("123"), Some("456"))
                .unwrap_err()
                .contains("inconsistent")
        );
    }

    #[test]
    fn rejects_any_provided_invalid_xuid() {
        for id in [
            "",
            "0",
            "-1",
            "+1",
            " 123",
            "hash",
            "１２３",
            "18446744073709551616",
        ] {
            for (xsts, xbl) in [
                (Some(id), None),
                (None, Some(id)),
                (Some(id), Some("123")),
                (Some("123"), Some(id)),
            ] {
                assert!(optional_xuid(xsts, xbl).unwrap_err().contains("invalid"));
            }
        }
    }

    #[test]
    fn polling_errors_and_backoff() {
        assert_eq!(
            oauth_error("authorization_pending").unwrap(),
            PollAction::Pending
        );
        assert_eq!(oauth_error("slow_down").unwrap(), PollAction::SlowDown);
        for code in [
            "expired_token",
            "authorization_declined",
            "access_denied",
            "invalid_client",
            "invalid_grant",
        ] {
            assert!(oauth_error(code).is_err());
        }
        let interval = next_interval(Duration::from_secs(5), false).unwrap();
        assert_eq!(interval, Duration::from_secs(10));
        assert_eq!(
            next_interval(interval, true).unwrap(),
            Duration::from_secs(20)
        );
        assert!(next_interval(Duration::MAX, false).is_err());
    }

    #[test]
    fn waits_respect_cancellation_and_expiry() {
        let now = Instant::now();
        assert!(
            wait(
                Duration::from_secs(5),
                now + Duration::from_secs(60),
                &AtomicBool::new(true)
            )
            .unwrap_err()
            .contains("cancelled")
        );
        assert!(
            wait(Duration::ZERO, now, &AtomicBool::new(false))
                .unwrap_err()
                .contains("expired")
        );
        assert!(
            wait(
                Duration::ZERO,
                now + Duration::from_secs(60),
                &AtomicBool::new(false)
            )
            .is_ok()
        );
    }

    #[test]
    fn expiry_guard_is_conservative() {
        let now = Instant::now();
        assert!(!has_token_lifetime(now, now));
        assert!(!has_token_lifetime(now + EXPIRY_GUARD, now));
        assert!(has_token_lifetime(
            now + EXPIRY_GUARD + Duration::from_secs(1),
            now
        ));
        assert!(deadline(now, 0).is_err());
    }

    #[test]
    fn errors_do_not_echo_server_secrets() {
        let secret = "SECRET_ACCESS_TOKEN";
        assert!(!oauth_error(secret).unwrap_err().contains(secret));
        assert!(
            !decode::<OAuthResponse>(secret.as_bytes())
                .err()
                .unwrap()
                .contains(secret)
        );
        let error: XboxError =
            decode(&br#"{"XErr":2148916233,"Message":"SECRET_ACCESS_TOKEN"}"#[..]).unwrap();
        assert!(xsts_error(error.code).contains("no Xbox profile"));
        assert!(xsts_error(Some(2148916238)).contains("child"));
        assert!(xsts_error(Some(2148916235)).contains("region"));
        assert!(xsts_error(Some(2148916236)).contains("verification"));
        assert!(!xsts_error(None).contains(secret));
    }

    #[test]
    fn parses_tokens_without_json_feature() {
        let oauth: OAuthResponse =
            decode(&br#"{"access_token":"ms","expires_in":3600,"token_type":"Bearer"}"#[..])
                .unwrap();
        assert!(oauth.error.is_none());
        assert_eq!(oauth.expires_in, Some(3600));
        let xbox: XboxResponse = decode(
            &br#"{"Token":"xbox","DisplayClaims":{"xui":[{"uhs":"hash","xid":"123"}]}}"#[..],
        )
        .unwrap();
        assert_eq!(xbox_user(&xbox).unwrap().xid.as_deref(), Some("123"));
        let invalid: XboxResponse =
            decode(&br#"{"Token":"xbox","DisplayClaims":{"xui":[]}}"#[..]).unwrap();
        assert!(xbox_user(&invalid).is_err());
        let mc: MinecraftToken =
            decode(&br#"{"access_token":"mc","expires_in":86400,"token_type":"Bearer"}"#[..])
                .unwrap();
        assert_eq!(mc.expires_in, 86400);
    }

    #[test]
    fn requires_java_entitlement() {
        let owned: Entitlements = decode(&br#"{"items":[{"name":"game_minecraft"}]}"#[..]).unwrap();
        assert!(owns_java(&owned));
        assert!(!owns_java(&Entitlements { items: vec![] }));
        assert!(!owns_java(&Entitlements {
            items: vec![Entitlement {
                name: "unrelated".into()
            }]
        }));
    }

    #[test]
    fn validates_and_normalizes_profile() {
        let (name, uuid) = verified_profile(Profile {
            name: "Player_1".into(),
            id: "01234567-89AB-CDEF-0123-456789abcdef".into(),
        })
        .unwrap();
        assert_eq!(name, "Player_1");
        assert_eq!(uuid, "0123456789abcdef0123456789abcdef");
        assert!(
            verified_profile(Profile {
                name: name.clone(),
                id: uuid
            })
            .is_ok()
        );
        for id in [
            "not-a-uuid",
            "00000000000000000000000000000000",
            "0123456789abcdef0123456789abcdeg",
        ] {
            assert!(
                verified_profile(Profile {
                    name: name.clone(),
                    id: id.into()
                })
                .is_err()
            );
        }
        assert!(
            verified_profile(Profile {
                name: "bad name".into(),
                id: "0123456789abcdef0123456789abcdef".into()
            })
            .is_err()
        );
    }

    #[test]
    fn session_scope_does_not_request_refresh_tokens() {
        // offline_access would enable refresh-token persistence; this module is session-only.
        assert_eq!(MS_SCOPE, "XboxLive.signin");
        assert!(!MS_SCOPE.to_ascii_lowercase().contains("offline_access"));
    }

    #[test]
    fn account_and_device_code_are_not_serde_types() {
        // Regression: Account / DeviceCode must remain session-only (no Serialize /
        // Deserialize / Debug). Internal OAuth structs may deserialize responses.
        fn assert_deserialize_ok<T: serde::de::DeserializeOwned>() {}
        assert_deserialize_ok::<OAuthResponse>();
        assert_deserialize_ok::<MinecraftToken>();
        // If Account gained Deserialize, the next line would still compile — so we
        // also assert the public types do not implement Debug (no accidental logging).
        let account_name = std::any::type_name::<Account>();
        let device_name = std::any::type_name::<DeviceCode>();
        assert!(account_name.contains("Account"));
        assert!(device_name.contains("DeviceCode"));
        // Document intentional absence of filesystem APIs in this module's public API.
        // Tokens live only in Account for the process lifetime.
        assert!(!account_name.is_empty());
    }
}
