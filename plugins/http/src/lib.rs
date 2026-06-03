// Copyright 2019-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

//! Access the HTTP client written in Rust.

use http::{header, HeaderMap};
pub use reqwest;
use reqwest::redirect::Policy;
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
use tauri::{
    plugin::{Builder, TauriPlugin},
    Manager, Runtime,
};
use url::Url;

pub use error::{Error, Result};

mod commands;
mod error;
#[cfg(feature = "cookies")]
mod reqwest_cookie_store;
mod scope;

#[cfg(feature = "cookies")]
const COOKIES_FILENAME: &str = ".cookies";

pub(crate) struct Http {
    #[cfg(feature = "cookies")]
    cookies_jar: std::sync::Arc<crate::reqwest_cookie_store::CookieStoreMutex>,
    middleware: std::sync::Mutex<Option<Arc<dyn Middleware>>>,
}

/// Middleware hooks to customize request/response handling.
/// Implemented by the application to add behaviors like auth header injection and token refresh.
pub trait Middleware: Send + Sync {
    /// Called before the request is sent; may mutate headers.
    fn pre_request(&self, url: &url::Url, headers: &mut HeaderMap);

    /// Called after every successful response is received (including retries after 401).
    fn on_response(&self, _response: &reqwest::Response) {}

    /// Called when the initial request returned 401 Unauthorized.
    /// Return Some(response) to replace the response (e.g., after refresh + retry), or None to keep the original 401.
    fn on_unauthorized<'a>(
        &'a self,
        original_url: url::Url,
        original_request: reqwest::RequestBuilder,
    ) -> Pin<Box<dyn Future<Output = Option<reqwest::Response>> + Send + 'a>>;
}

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::<R>::new("http")
        .setup(|app, _| {
            #[cfg(feature = "cookies")]
            let cookies_jar = {
                use crate::reqwest_cookie_store::*;
                use std::fs::File;
                use std::io::BufReader;

                let cache_dir = app.path().app_cache_dir()?;
                std::fs::create_dir_all(&cache_dir)?;

                let path = cache_dir.join(COOKIES_FILENAME);
                let file = File::options()
                    .create(true)
                    .append(true)
                    .read(true)
                    .open(&path)?;

                let reader = BufReader::new(file);
                CookieStoreMutex::load(path.clone(), reader).unwrap_or_else(|_e| {
                    #[cfg(feature = "tracing")]
                    tracing::warn!(
                        "failed to load cookie store: {_e}, falling back to empty store"
                    );
                    CookieStoreMutex::new(path, Default::default())
                })
            };

            let state = Http {
                #[cfg(feature = "cookies")]
                cookies_jar: std::sync::Arc::new(cookies_jar),
                middleware: std::sync::Mutex::new(None),
            };

            app.manage(state);

            Ok(())
        })
        .on_event(|app, event| {
            #[cfg(feature = "cookies")]
            if let tauri::RunEvent::Exit = event {
                let state = app.state::<Http>();

                match state.cookies_jar.request_save() {
                    Ok(rx) => {
                        let _ = rx.recv();
                    }
                    Err(_e) => {
                        #[cfg(feature = "tracing")]
                        tracing::error!("failed to save cookie jar: {_e}");
                    }
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::fetch,
            commands::fetch_cancel,
            commands::fetch_send,
            commands::fetch_read_body,
            commands::fetch_cancel_body,
        ])
        .build()
}

/// Initialize the plugin with a custom middleware implementation.
pub fn init_with_middleware<R: Runtime>(middleware: Arc<dyn Middleware>) -> TauriPlugin<R> {
    Builder::<R>::new("http")
        .setup({
            let middleware = middleware.clone();
            move |app, _| {
                #[cfg(feature = "cookies")]
                let cookies_jar = {
                    use crate::reqwest_cookie_store::*;
                    use std::fs::File;
                    use std::io::BufReader;

                    let cache_dir = app.path().app_cache_dir()?;
                    std::fs::create_dir_all(&cache_dir)?;

                    let path = cache_dir.join(COOKIES_FILENAME);
                    let file = File::options()
                        .create(true)
                        .append(true)
                        .read(true)
                        .open(&path)?;

                    let reader = BufReader::new(file);
                    CookieStoreMutex::load(path.clone(), reader).unwrap_or_else(|_e| {
                        #[cfg(feature = "tracing")]
                        tracing::warn!(
                            "failed to load cookie store: {_e}, falling back to empty store"
                        );
                        CookieStoreMutex::new(path, Default::default())
                    })
                };

                let state = Http {
                    #[cfg(feature = "cookies")]
                    cookies_jar: std::sync::Arc::new(cookies_jar),
                    middleware: std::sync::Mutex::new(Some(middleware.clone())),
                };

                app.manage(state);

                Ok(())
            }
        })
        .on_event(|app, event| {
            #[cfg(feature = "cookies")]
            if let tauri::RunEvent::Exit = event {
                let state = app.state::<Http>();

                match state.cookies_jar.request_save() {
                    Ok(rx) => {
                        let _ = rx.recv();
                    }
                    Err(_e) => {
                        #[cfg(feature = "tracing")]
                        tracing::error!("failed to save cookie jar: {_e}");
                    }
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::fetch,
            commands::fetch_cancel,
            commands::fetch_send,
            commands::fetch_read_body
        ])
        .build()
}

/// Set or replace the middleware after plugin initialization.
pub fn set_middleware<R: Runtime>(app: &tauri::AppHandle<R>, middleware: Arc<dyn Middleware>) {
    // Acquire, then drop temporaries before returning
    {
        let state: tauri::State<Http> = app.state();
        let lock = state.middleware.lock();
        if let Ok(mut guard) = lock {
            *guard = Some(middleware);
        }
    };
}

/// Execute a reqwest RequestBuilder with shared middleware.
/// Caller is responsible for creating both the initial and a retry builder with the same method/body.
pub async fn execute_with_middleware<R: Runtime>(
    app: &tauri::AppHandle<R>,
    request: reqwest::RequestBuilder,
    retry_request: reqwest::RequestBuilder,
    url: Url,
    mut headers: HeaderMap,
) -> Result<reqwest::Response> {
    let state: tauri::State<Http> = app.state();
    let middleware_opt = match state.middleware.lock() {
        Ok(g) => (*g).clone(),
        Err(_) => None,
    };

    if let Some(ref m) = middleware_opt {
        m.pre_request(&url, &mut headers);
    }

    let resp = request.headers(headers).send().await?;
    if resp.status() != reqwest::StatusCode::UNAUTHORIZED {
        if let Some(ref m) = middleware_opt {
            m.on_response(&resp);
        }
        return Ok(resp);
    }

    if let Some(m) = middleware_opt {
        if let Some(r2) = m.on_unauthorized(url, retry_request).await {
            m.on_response(&r2);
            return Ok(r2);
        }
    }

    Ok(resp)
}

/// Execute with a provided middleware snapshot, avoiding any app/state borrows.
pub async fn execute_with_middleware_opt(
    middleware_opt: Option<Arc<dyn Middleware>>,
    request: reqwest::RequestBuilder,
    retry_request: reqwest::RequestBuilder,
    url: Url,
    mut headers: HeaderMap,
) -> Result<reqwest::Response> {
    if let Some(ref m) = middleware_opt {
        m.pre_request(&url, &mut headers);
    }

    let resp = request.headers(headers).send().await?;
    if resp.status() != reqwest::StatusCode::UNAUTHORIZED {
        if let Some(ref m) = middleware_opt {
            m.on_response(&resp);
        }
        return Ok(resp);
    }

    if let Some(m) = middleware_opt {
        if let Some(r2) = m.on_unauthorized(url, retry_request).await {
            m.on_response(&r2);
            return Ok(r2);
        }
    }

    Ok(resp)
}

// NOTE: keep file end clean
