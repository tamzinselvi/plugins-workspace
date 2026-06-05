// Copyright 2019-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

//! Access the HTTP client written in Rust.

use http::HeaderMap;
pub use reqwest;
pub use reqwest_middleware;
use reqwest_middleware::{
    ClientBuilder as MiddlewareClientBuilder, ClientWithMiddleware, Middleware,
};
use std::sync::Arc;
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
    middleware_layers: std::sync::Mutex<Vec<Arc<dyn Middleware>>>,
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
                middleware_layers: std::sync::Mutex::new(Vec::new()),
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

/// Register a reqwest middleware layer applied to all HTTP requests made through this plugin.
/// Layers are applied in registration order (first registered = outermost in the chain).
pub fn add_middleware_layer<R: Runtime>(app: &tauri::AppHandle<R>, layer: Arc<dyn Middleware>) {
    {
        let state: tauri::State<Http> = app.state();
        let lock = state.middleware_layers.lock();
        if let Ok(mut layers) = lock {
            layers.push(layer);
        }
    };
}

/// Wrap a reqwest::Client with all registered middleware layers.
/// Use this to build every client so middleware fires consistently across all requests.
pub fn build_client_with_middleware<R: Runtime>(
    app: &tauri::AppHandle<R>,
    base: reqwest::Client,
) -> ClientWithMiddleware {
    let layers: Vec<Arc<dyn Middleware>> = {
        let state: tauri::State<Http> = app.state();
        state
            .middleware_layers
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    };
    let mut builder = MiddlewareClientBuilder::new(base);
    for layer in layers {
        builder = builder.with_arc(layer);
    }
    builder.build()
}

/// Execute a request through the registered middleware chain.
/// Build the request via [`build_client_with_middleware`] so middleware fires on `.send()`.
pub async fn execute_with_middleware<R: Runtime>(
    _app: &tauri::AppHandle<R>,
    request: reqwest_middleware::RequestBuilder,
    _url: Url,
    headers: HeaderMap,
) -> Result<reqwest::Response> {
    Ok(request.headers(headers).send().await?)
}

// NOTE: keep file end clean
