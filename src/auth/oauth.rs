//! Local OAuth server for handling ChatGPT login flow

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};
use tiny_http::{Request, Response, Server};
use tokio::sync::oneshot;

use crate::types::{parse_chatgpt_id_token_claims, OAuthLoginInfo, StoredAccount};

const DEFAULT_ISSUER: &str = "https://auth.openai.com";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const DEFAULT_PORT: u16 = 1455; // Same as official Codex

/// PKCE codes for OAuth
#[derive(Debug, Clone)]
pub struct PkceCodes {
    pub code_verifier: String,
    pub code_challenge: String,
}

/// Generate PKCE codes
pub fn generate_pkce() -> PkceCodes {
    let mut bytes = [0u8; 64];
    rand::rng().fill_bytes(&mut bytes);

    let code_verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let digest = Sha256::digest(code_verifier.as_bytes());
    let code_challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);

    PkceCodes {
        code_verifier,
        code_challenge,
    }
}

/// Generate a random state parameter
fn generate_state() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Build the OAuth authorization URL
fn build_authorize_url(
    issuer: &str,
    client_id: &str,
    redirect_uri: &str,
    pkce: &PkceCodes,
    state: &str,
) -> String {
    let params = [
        ("response_type", "code"),
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("scope", "openid profile email offline_access"),
        ("code_challenge", &pkce.code_challenge),
        ("code_challenge_method", "S256"),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("state", state),
        ("originator", "codex_cli_rs"), // Required by OpenAI OAuth
    ];

    let query_string = params
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&");

    format!("{issuer}/oauth/authorize?{query_string}")
}

/// Token response from the OAuth server
#[derive(Debug, Clone, serde::Deserialize)]
struct TokenResponse {
    id_token: String,
    access_token: String,
    refresh_token: String,
}

/// Exchange authorization code for tokens
async fn exchange_code_for_tokens(
    issuer: &str,
    client_id: &str,
    redirect_uri: &str,
    pkce: &PkceCodes,
    code: &str,
) -> Result<TokenResponse> {
    let client = reqwest::Client::new();

    let body = format!(
        "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
        urlencoding::encode(code),
        urlencoding::encode(redirect_uri),
        urlencoding::encode(client_id),
        urlencoding::encode(&pkce.code_verifier)
    );

    let resp = client
        .post(format!("{issuer}/oauth/token"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .context("Failed to send token request")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Token exchange failed: {status} - {body}");
    }

    let tokens: TokenResponse = resp
        .json()
        .await
        .context("Failed to parse token response")?;
    Ok(tokens)
}

/// OAuth login flow result
pub struct OAuthLoginResult {
    pub account: StoredAccount,
}

/// Start the OAuth login flow
pub async fn start_oauth_login(
    account_name: String,
) -> Result<(
    OAuthLoginInfo,
    oneshot::Receiver<Result<OAuthLoginResult>>,
    Arc<AtomicBool>,
)> {
    let pkce = generate_pkce();
    let state = generate_state();

    // Try official default port first; fall back to a random free port if it is busy.
    let server = match Server::http(format!("127.0.0.1:{DEFAULT_PORT}")) {
        Ok(server) => server,
        Err(default_err) => {
            eprintln!(
                "[OAuth] Default callback port {DEFAULT_PORT} unavailable ({default_err}), using a random local port"
            );
            Server::http("127.0.0.1:0").map_err(|fallback_err| {
                anyhow::anyhow!(
                    "Failed to start OAuth server: default port {DEFAULT_PORT} error: {default_err}; fallback error: {fallback_err}"
                )
            })?
        }
    };

    let actual_port = match server.server_addr().to_ip() {
        Some(addr) => addr.port(),
        None => anyhow::bail!("Failed to determine server port"),
    };

    let redirect_uri = format!("http://localhost:{actual_port}/auth/callback");
    let auth_url = build_authorize_url(DEFAULT_ISSUER, CLIENT_ID, &redirect_uri, &pkce, &state);

    let login_info = OAuthLoginInfo {
        auth_url: auth_url.clone(),
        callback_port: actual_port,
    };

    // Create a channel for the result
    let (tx, rx) = oneshot::channel();
    let cancelled = Arc::new(AtomicBool::new(false));

    // Spawn the server in a background thread
    let server = Arc::new(server);
    let pkce_clone = pkce.clone();
    let state_clone = state.clone();
    let cancelled_clone = cancelled.clone();

    thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let result = runtime.block_on(run_oauth_server(
            server,
            pkce_clone,
            state_clone,
            redirect_uri,
            account_name,
            cancelled_clone,
        ));
        let _ = tx.send(result);
    });

    Ok((login_info, rx, cancelled))
}

/// Run the OAuth callback server
async fn run_oauth_server(
    server: Arc<Server>,
    pkce: PkceCodes,
    expected_state: String,
    redirect_uri: String,
    account_name: String,
    cancelled: Arc<AtomicBool>,
) -> Result<OAuthLoginResult> {
    let timeout = Duration::from_secs(300); // 5 minute timeout
    let start = std::time::Instant::now();

    loop {
        if cancelled.load(Ordering::Relaxed) {
            anyhow::bail!("OAuth login cancelled");
        }

        if start.elapsed() > timeout {
            anyhow::bail!("OAuth login timed out");
        }

        // Use recv_timeout to allow checking the timeout
        let request = match server.recv_timeout(Duration::from_secs(1)) {
            Ok(Some(req)) => req,
            Ok(None) => continue,
            Err(_) => continue,
        };

        let result = handle_oauth_request(
            request,
            &pkce,
            &expected_state,
            &redirect_uri,
            &account_name,
        )
        .await;

        match result {
            HandleResult::Continue => continue,
            HandleResult::Success(account) => {
                server.unblock();
                return Ok(OAuthLoginResult { account: *account });
            }
            HandleResult::Error(e) => {
                server.unblock();
                return Err(e);
            }
        }
    }
}

enum HandleResult {
    Continue,
    Success(Box<StoredAccount>),
    Error(anyhow::Error),
}

async fn handle_oauth_request(
    request: Request,
    pkce: &PkceCodes,
    expected_state: &str,
    redirect_uri: &str,
    account_name: &str,
) -> HandleResult {
    let url_str = request.url().to_string();
    let parsed = match url::Url::parse(&format!("http://localhost{url_str}")) {
        Ok(u) => u,
        Err(_) => {
            let _ = request.respond(Response::from_string("Bad Request").with_status_code(400));
            return HandleResult::Continue;
        }
    };

    let path = parsed.path();

    if path == "/auth/callback" {
        let params: std::collections::HashMap<String, String> =
            parsed.query_pairs().into_owned().collect();

        // Verify state
        if params.get("state").map(String::as_str) != Some(expected_state) {
            let _ = request.respond(Response::from_string("State mismatch").with_status_code(400));
            return HandleResult::Continue;
        }

        // Check for error response
        if let Some(error) = params.get("error") {
            let error_desc = params
                .get("error_description")
                .map(|s| s.as_str())
                .unwrap_or("Unknown error");
            let _ = request.respond(
                Response::from_string(format!("OAuth Error: {error} - {error_desc}"))
                    .with_status_code(400),
            );
            return HandleResult::Error(anyhow::anyhow!("OAuth error: {error} - {error_desc}"));
        }

        // Get the authorization code
        let code = match params.get("code") {
            Some(c) if !c.is_empty() => c.clone(),
            _ => {
                let _ = request.respond(
                    Response::from_string("Missing authorization code").with_status_code(400),
                );
                return HandleResult::Error(anyhow::anyhow!("Missing authorization code"));
            }
        };

        // Exchange code for tokens
        match exchange_code_for_tokens(DEFAULT_ISSUER, CLIENT_ID, redirect_uri, pkce, &code).await {
            Ok(tokens) => {
                // Parse claims from ID token
                let claims = parse_chatgpt_id_token_claims(&tokens.id_token);

                // Create the account
                let account = StoredAccount::new_chatgpt(
                    account_name.to_string(),
                    claims.email,
                    claims.plan_type,
                    claims.subscription_expires_at,
                    tokens.id_token,
                    tokens.access_token,
                    tokens.refresh_token,
                    claims.account_id,
                );

                let _ = request.respond(Response::from_string(
                    "Login complete. You can close this tab and return to the terminal.",
                ));

                return HandleResult::Success(Box::new(account));
            }
            Err(e) => {
                let _ = request.respond(
                    Response::from_string(format!("Token exchange failed: {e}"))
                        .with_status_code(500),
                );
                return HandleResult::Error(e);
            }
        }
    }

    // Handle other paths
    let _ = request.respond(Response::from_string("Not Found").with_status_code(404));
    HandleResult::Continue
}

/// Wait for the OAuth login to complete
pub async fn wait_for_oauth_login(
    rx: oneshot::Receiver<Result<OAuthLoginResult>>,
) -> Result<StoredAccount> {
    let result = rx.await.context("OAuth login was cancelled")??;
    Ok(result.account)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;

    #[test]
    fn pkce_challenge_is_sha256_of_a_random_verifier() {
        let pkce = generate_pkce();
        assert!((43..=128).contains(&pkce.code_verifier.len()));
        assert_eq!(
            pkce.code_challenge,
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(Sha256::digest(pkce.code_verifier.as_bytes()))
        );
        assert_ne!(generate_pkce().code_verifier, pkce.code_verifier);
    }

    #[tokio::test]
    async fn wrong_state_is_rejected_without_ending_the_login() {
        let server = Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let client = thread::spawn(move || {
            let mut socket = TcpStream::connect(address).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            socket.write_all(b"GET /auth/callback?state=wrong&code=unused HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").unwrap();
            let mut response = String::new();
            socket.read_to_string(&mut response).unwrap();
            response
        });
        let request = server
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        let result = handle_oauth_request(
            request,
            &generate_pkce(),
            "expected",
            "http://localhost/auth/callback",
            "test",
        )
        .await;
        assert!(matches!(result, HandleResult::Continue));
        assert!(client.join().unwrap().starts_with("HTTP/1.1 400"));
    }

    #[tokio::test]
    async fn cancellation_releases_the_callback_listener() {
        let server = Arc::new(Server::http("127.0.0.1:0").unwrap());
        let address = server.server_addr().to_ip().unwrap();
        let result = run_oauth_server(
            server,
            generate_pkce(),
            "state".into(),
            "http://localhost/auth/callback".into(),
            "test".into(),
            Arc::new(AtomicBool::new(true)),
        )
        .await;
        assert!(result.err().unwrap().to_string().contains("cancelled"));
        // tiny_http wakes its accept thread in Drop without joining it.
        // Allow that thread to release the socket before rebinding.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if Server::http(address).is_ok() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "callback listener was not released"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
