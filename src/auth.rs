//! YouTube account sign-in.
//!
//! rustypipe authenticates library requests with the account's YouTube
//! cookies. Two ways to get them:
//!
//! 1. **Login window**: a private (incognito) webview where the user signs in
//!    with Google. Once Google redirects back to youtube.com we copy the
//!    cookies and destroy the window, so nothing else keeps using (and
//!    rotating) that session.
//! 2. **cookies.txt import** from a regular browser, for when Google refuses
//!    to sign in inside an embedded webview.

use rustypipe::client::RustyPipe;
use serde::Serialize;
use tauri::webview::PageLoadEvent;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

const LOGIN_LABEL: &str = "login";
/// Google only accepts real YouTube pages as the post-login redirect, so we
/// land on music.youtube.com and grab the cookies as soon as that page *starts*
/// loading, before YouTube's scripts run and begin rotating them.
const LOGIN_URL: &str = "https://accounts.google.com/ServiceLogin?service=youtube&uilel=3&passive=true&continue=https%3A%2F%2Fwww.youtube.com%2Fsignin%3Faction_handle_signin%3Dtrue%26app%3Ddesktop%26next%3Dhttps%253A%252F%252Fmusic.youtube.com%252F";

/// True once Google has redirected back to a YouTube page (not the signin handler).
fn is_done(url: &tauri::Url) -> bool {
    matches!(url.host_str(), Some("music.youtube.com" | "www.youtube.com")) && !url.path().starts_with("/signin")
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthStatus {
    pub logged_in: bool,
    pub error: Option<String>,
}

pub fn emit(app: &AppHandle, logged_in: bool, error: Option<String>) {
    let _ = app.emit("auth", AuthStatus { logged_in, error });
}

/// Whether a YouTube session cookie is stored (no network request).
pub fn has_session(rp: &RustyPipe) -> bool {
    rp.query().auth_enabled(rustypipe::client::ClientType::DesktopMusic)
}

pub fn open_login_window(app: &AppHandle, rp: RustyPipe, user_agent: Option<String>) -> tauri::Result<()> {
    if let Some(w) = app.get_webview_window(LOGIN_LABEL) {
        let _ = w.set_focus();
        return Ok(());
    }
    let url = LOGIN_URL.parse().expect("valid login url");
    // Redirects can report several YouTube pages; take the cookies once.
    let taken = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut builder = WebviewWindowBuilder::new(app, LOGIN_LABEL, WebviewUrl::External(url))
        .title("Sign in to YouTube")
        .inner_size(480.0, 720.0)
        .incognito(true)
        .on_page_load(move |webview, payload| {
            if payload.event() != PageLoadEvent::Started || !is_done(payload.url()) {
                return;
            }
            if taken.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            log::info!("login redirected back to {}", payload.url().host_str().unwrap_or_default());
            let (rp, app) = (rp.clone(), webview.app_handle().clone());
            let window = webview.clone();
            // Cookie access must not happen on the event-loop thread (deadlocks on Windows).
            tauri::async_runtime::spawn(async move {
                let cookies = window.cookies_for_url("https://www.youtube.com/".parse().unwrap());
                let _ = window.destroy();
                if let Ok(c) = &cookies {
                    log::debug!("login cookies: {}", c.iter().map(|c| c.name()).collect::<Vec<_>>().join(", "));
                }
                let result = match cookies {
                    Ok(c) => {
                        let header = c
                            .iter()
                            .map(|c| format!("{}={}", c.name(), c.value()))
                            .collect::<Vec<_>>()
                            .join("; ");
                        if !header.contains("SAPISID") {
                            Err("sign-in did not complete (no session cookie)".to_string())
                        } else {
                            rp.user_auth_set_cookie(header).await.map_err(|e| e.to_string())
                        }
                    }
                    Err(e) => Err(format!("could not read cookies: {e}")),
                };
                match result {
                    Ok(()) => {
                        log::info!("signed in");
                        emit(&app, true, None);
                    }
                    Err(e) => {
                        log::warn!("sign-in failed: {e}");
                        emit(&app, false, Some(e));
                    }
                }
            });
        });
    if let Some(ua) = user_agent {
        builder = builder.user_agent(&ua);
    }
    builder.build()?;
    Ok(())
}

pub async fn import_cookies_txt(rp: &RustyPipe, text: &str) -> Result<(), String> {
    rp.user_auth_set_cookie_txt(text).await.map_err(|e| e.to_string())
}

pub async fn logout(rp: &RustyPipe) {
    let _ = rp.user_auth_remove_cookie().await;
}
