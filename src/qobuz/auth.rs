use std::collections::HashSet;
use std::sync::LazyLock;

use base64::Engine as _;
use regex::Regex;
use serde::Deserialize;

use super::client::Client;
use super::types::{QobuzError, Result, Status, WEB_PLAYER_LOGIN, md5_hex};

static RE_BUNDLE_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<script src="(/resources/\d+\.\d+\.\d+-[a-z]\d+/bundle\.js)"></script>"#).unwrap()
});
static RE_APP_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"production:\{api:\{appId:"(\d+)",appSecret:"([^"]*)""#).unwrap()
});
static RE_SEED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"[a-z]\.initialSeed\("([\w=]+)",window\.utimezone\.([a-z]+)\)"#).unwrap()
});
static RE_PRIVATE_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"authenticate\(\{privateKey:"([^"]+)""#).unwrap());

#[derive(Deserialize)]
struct OAuthResp {
    #[serde(default)]
    token: String,
}

#[derive(Deserialize)]
struct LoginUserSub {
    #[serde(default)]
    offer: String,
}

#[derive(Deserialize)]
struct LoginUser {
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    login: String,
    #[serde(default)]
    subscription: Option<LoginUserSub>,
}

#[derive(Deserialize)]
struct LoginResp {
    #[serde(default)]
    user: Option<LoginUser>,
    #[serde(default)]
    user_auth_token: String,
}

impl Client {
    pub async fn oauth_url(&self, callback_url: &str) -> Result<String> {
        self.ensure_app_credentials().await?;
        let app_id = self.app_id();
        let mut u =
            url::Url::parse("https://www.qobuz.com/signin/oauth").expect("valid oauth base url");
        u.query_pairs_mut()
            .append_pair("ext_app_id", &app_id)
            .append_pair("redirect_url", callback_url);
        Ok(u.to_string())
    }

    pub async fn exchange_oauth_code(&self, code: &str) -> Result<Status> {
        let code = code.trim();
        if code.is_empty() {
            return Err(QobuzError::Other("missing OAuth authorization code".into()));
        }
        self.ensure_app_credentials().await?;
        let private_key = self.private_key();
        if private_key.is_empty() {
            return Err(QobuzError::Other(
                "qobuz OAuth private_key was not found in web player bundle".into(),
            ));
        }

        let resp: OAuthResp = self
            .get_json(
                "/oauth/callback",
                &[("code", code), ("private_key", &private_key)],
                false,
            )
            .await
            .map_err(|e| QobuzError::Other(format!("qobuz oauth exchange failed: {e}")))?;
        if resp.token.trim().is_empty() {
            return Err(QobuzError::Other(
                "qobuz oauth exchange returned empty token".into(),
            ));
        }
        self.login_with_token(&resp.token).await
    }

    pub async fn login_with_token(&self, token: &str) -> Result<Status> {
        let token = token.trim();
        if token.is_empty() {
            return Err(QobuzError::Other("user_auth_token is required".into()));
        }
        self.ensure_app_credentials().await?;

        let resp: LoginResp = self
            .post_form_json_with_token("/user/login", &[("extra", "partner")], token)
            .await
            .map_err(|e| QobuzError::Other(format!("qobuz token validation failed: {e}")))?;

        let (name, sub) = extract_user_info(resp.user);
        let final_token = if resp.user_auth_token.is_empty() {
            token.to_string()
        } else {
            resp.user_auth_token
        };
        self.apply_login(final_token, name, sub);
        Ok(self.status())
    }

    pub async fn login(&self, email: &str, password: &str) -> Result<Status> {
        let email = email.trim();
        if email.is_empty() || password.is_empty() {
            return Err(QobuzError::Other("email and password are required".into()));
        }
        self.ensure_app_credentials().await?;
        let pw_hash = md5_hex(password);
        let resp: LoginResp = self
            .get_json(
                "/user/login",
                &[("email", email), ("password", &pw_hash)],
                false,
            )
            .await
            .map_err(|e| QobuzError::Other(format!("qobuz login failed: {e}")))?;
        if resp.user_auth_token.is_empty() {
            return Err(QobuzError::Other(
                "qobuz login returned empty auth token".into(),
            ));
        }
        let (name, sub) = extract_user_info(resp.user);
        self.apply_login(resp.user_auth_token, name, sub);
        Ok(self.status())
    }

    pub(crate) async fn ensure_app_credentials(&self) -> Result<()> {
        let (have_id, have_secret) = self.has_app_credentials();
        if have_id && have_secret {
            return Ok(());
        }
        if have_id != have_secret {
            return Err(QobuzError::Other(
                "both QOBUZ_APP_ID and QOBUZ_APP_SECRET must be set together (or omit both for auto-discovery)".into(),
            ));
        }
        let (app_id, secrets, priv_key) = self
            .discover_web_player_credentials()
            .await
            .map_err(|e| {
                QobuzError::Other(format!(
                    "could not auto-discover Qobuz web player credentials (set QOBUZ_APP_ID & QOBUZ_APP_SECRET manually): {e}"
                ))
            })?;
        self.set_discovered_app_credentials(app_id, secrets, priv_key);
        Ok(())
    }

    async fn discover_web_player_credentials(&self) -> Result<(String, Vec<String>, String)> {
        let html = self.fetch_text(WEB_PLAYER_LOGIN).await?;
        let bundle_path = RE_BUNDLE_PATH
            .captures(&html)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())
            .ok_or_else(|| {
                QobuzError::Other("bundle.js path not found on play.qobuz.com/login".into())
            })?;
        let bundle_body = self
            .fetch_text(&format!("https://play.qobuz.com{bundle_path}"))
            .await?;

        let id_caps = RE_APP_ID
            .captures(&bundle_body)
            .ok_or_else(|| QobuzError::Other("app_id not found in Qobuz web bundle".into()))?;
        let app_id = id_caps[1].to_string();
        let literal_secret = id_caps
            .get(2)
            .map(|m| m.as_str().trim().to_string())
            .unwrap_or_default();

        let private_key = RE_PRIVATE_KEY
            .captures(&bundle_body)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();

        let mut seen = HashSet::new();
        let mut secrets = Vec::new();
        let mut add_secret = |s: String| {
            if !s.is_empty() && seen.insert(s.clone()) {
                secrets.push(s);
            }
        };

        for caps in RE_SEED.captures_iter(&bundle_body) {
            let seed = &caps[1];
            let tz = &caps[2];
            let mut chars = tz.chars();
            let Some(first) = chars.next() else { continue };
            let tz_title = format!("{}{}", first.to_ascii_uppercase(), chars.as_str());
            let pat = format!(
                r#"name:"\w+/{}",info:"([\w=]+)",extras:"([\w=]+)""#,
                regex::escape(&tz_title)
            );
            let Ok(info_re) = Regex::new(&pat) else {
                continue;
            };
            let Some(im) = info_re.captures(&bundle_body) else {
                continue;
            };
            let combined = format!("{seed}{}{}", &im[1], &im[2]);
            if combined.len() <= 44 {
                continue;
            }
            let b64 = &combined[..combined.len() - 44];
            if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(b64)
                && !decoded.is_empty()
                && let Ok(s) = String::from_utf8(decoded)
            {
                add_secret(s);
            }
        }
        add_secret(literal_secret);

        if secrets.is_empty() {
            return Err(QobuzError::Other(
                "no signing secrets extracted from Qobuz web bundle".into(),
            ));
        }
        Ok((app_id, secrets, private_key))
    }
}

fn extract_user_info(user: Option<LoginUser>) -> (String, String) {
    let Some(u) = user else {
        return (String::new(), String::new());
    };
    let name = if !u.display_name.is_empty() {
        u.display_name
    } else {
        u.login
    };
    let sub = u.subscription.map(|s| s.offer).unwrap_or_default();
    (name, sub)
}
