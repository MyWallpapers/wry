// Copyright 2020-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

//! Unix platform extensions for [`WebContext`](super::WebContext).

use crate::{Error, RequestAsyncResponder};
use gtk::glib::{self, MainContext, ObjectExt};
use http::{header::CONTENT_TYPE, HeaderName, HeaderValue, Request, Response as HttpResponse};
use soup::{MessageHeaders, MessageHeadersType};
use std::{
  borrow::Cow,
  cell::{Cell, RefCell},
  env::current_dir,
  path::{Path, PathBuf},
  rc::Rc,
};
use webkit2gtk::{
  ApplicationInfo, AutomationSessionExt, Download, DownloadExt, SecurityManagerExt, URIRequest,
  URIRequestExt, URIResponseExt, URISchemeRequestExt, URISchemeResponse, URISchemeResponseExt,
  WebContext, WebContextExt as Webkit2gtkContextExt, WebView, WebViewExt,
};

const MAX_ACTIVE_DOWNLOADS: usize = 4;
const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug)]
pub struct WebContextImpl {
  context: WebContext,
  automation: bool,
  app_info: Option<ApplicationInfo>,
}

impl WebContextImpl {
  pub fn new(data_directory: Option<&Path>) -> Self {
    use webkit2gtk::{WebsiteDataManager, WebsiteDataManagerExt};
    let mut context_builder = WebContext::builder();
    if let Some(data_directory) = data_directory {
      let data_manager = WebsiteDataManager::builder()
        .base_data_directory(data_directory.to_string_lossy())
        .build();
      // Persist website data (IndexedDB/local storage/cache) under the app data
      // directory, but keep cookies in-memory only on WebKitGTK. This avoids
      // cross-session cookie retention without disabling the rest of the web data
      // manager.
      context_builder = context_builder.website_data_manager(&data_manager);
    }
    let context = context_builder.build();

    Self::create_context(context)
  }

  pub fn new_ephemeral() -> Self {
    let context = WebContext::new_ephemeral();

    Self::create_context(context)
  }

  pub fn create_context(context: WebContext) -> Self {
    let automation = false;
    context.set_automation_allowed(automation);

    // e.g. wry 0.9.4
    let app_info = ApplicationInfo::new();
    app_info.set_name(env!("CARGO_PKG_NAME"));
    app_info.set_version(
      env!("CARGO_PKG_VERSION_MAJOR")
        .parse()
        .expect("invalid wry version major"),
      env!("CARGO_PKG_VERSION_MINOR")
        .parse()
        .expect("invalid wry version minor"),
      env!("CARGO_PKG_VERSION_PATCH")
        .parse()
        .expect("invalid wry version patch"),
    );

    Self {
      context,
      automation,
      app_info: Some(app_info),
    }
  }

  pub fn set_allows_automation(&mut self, flag: bool) {
    self.automation = flag;
    self.context.set_automation_allowed(flag);
  }

  pub fn set_web_extensions_directory(&mut self, path: &Path) {
    self
      .context
      .set_web_extensions_directory(&path.to_string_lossy());
  }
}

fn active_download_limit_reached(active_downloads: usize) -> bool {
  active_downloads >= MAX_ACTIVE_DOWNLOADS
}

fn download_content_length_exceeds_limit(content_length: u64) -> bool {
  content_length > MAX_DOWNLOAD_BYTES
}

fn download_received_data_exceeds_limit(received_data_length: u64) -> bool {
  received_data_length > MAX_DOWNLOAD_BYTES
}

fn release_download_slot(active_downloads: &Rc<Cell<usize>>, slot_released: &Rc<Cell<bool>>) {
  if slot_released.replace(true) {
    return;
  }

  active_downloads.set(active_downloads.get().saturating_sub(1));
}

fn cancel_download_if_response_too_large(
  download: &Download,
  active_downloads: &Rc<Cell<usize>>,
  slot_released: &Rc<Cell<bool>>,
) {
  let exceeds_limit = download
    .response()
    .map(|response| download_content_length_exceeds_limit(response.content_length()))
    .unwrap_or(false);

  if exceeds_limit {
    release_download_slot(active_downloads, slot_released);
    download.cancel();
  }
}

fn cancel_download_if_received_data_too_large(
  download: &Download,
  active_downloads: &Rc<Cell<usize>>,
  slot_released: &Rc<Cell<bool>>,
) {
  if download_received_data_exceeds_limit(download.received_data_length()) {
    release_download_slot(active_downloads, slot_released);
    download.cancel();
  }
}

/// [`WebContext`](super::WebContext) items that only matter on unix.
pub trait WebContextExt {
  /// The GTK [`WebContext`] of all webviews in the context.
  fn context(&self) -> &WebContext;

  /// Register a custom protocol to the web context.
  fn register_uri_scheme<F>(&mut self, name: &str, handler: F) -> crate::Result<()>
  where
    F: Fn(crate::WebViewId, Request<Vec<u8>>, RequestAsyncResponder) + 'static;

  /// Loads a URI for a [`WebView`].
  fn load_uri(&self, webview: WebView, url: String, headers: Option<http::HeaderMap>);

  /// If the context allows automation.
  ///
  /// **Note:** `libwebkit2gtk` only allows 1 automation context at a time.
  fn allows_automation(&self) -> bool;

  fn register_automation(&mut self, webview: WebView);

  fn register_download_handler(
    &mut self,
    download_started_callback: Option<Box<dyn FnMut(String, &mut PathBuf) -> bool>>,
    download_completed_callback: Option<Rc<dyn Fn(String, Option<PathBuf>, bool) + 'static>>,
  );
}

impl WebContextExt for super::WebContext {
  fn context(&self) -> &WebContext {
    &self.os.context
  }

  fn register_uri_scheme<F>(&mut self, name: &str, handler: F) -> crate::Result<()>
  where
    F: Fn(crate::WebViewId, Request<Vec<u8>>, RequestAsyncResponder) + 'static,
  {
    self.register_custom_protocol(name.to_owned())?;

    // Enable secure context
    self
      .os
      .context
      .security_manager()
      .ok_or(Error::MissingManager)?
      .register_uri_scheme_as_secure(name);

    self.os.context.register_uri_scheme(name, move |request| {
      #[cfg(feature = "tracing")]
      let span = tracing::info_span!(parent: None, "wry::custom_protocol::handle", uri = tracing::field::Empty).entered();

      if let Some(uri) = request.uri() {
        let uri = uri.as_str();

        #[cfg(feature = "tracing")]
        span.record("uri", uri);

        #[allow(unused_mut)]
        let mut http_request = Request::builder().uri(uri).method("GET");

        // Set request http headers
        if let Some(headers) = request.http_headers() {
          if let Some(map) = http_request.headers_mut() {
            headers.foreach(move |k, v| {
              if let Ok(name) = HeaderName::from_bytes(k.as_bytes()) {
                if let Ok(value) = HeaderValue::from_bytes(v.as_bytes()) {
                  map.insert(name, value);
                }
              }
            });
          }
        }

        // Set request http method
        if let Some(method) = request.http_method() {
          http_request = http_request.method(method.as_str());
        }

        let body;
        #[cfg(feature = "linux-body")]
        {
          use gtk::{gdk::prelude::InputStreamExtManual, gio::Cancellable};

          // Set request http body
          let cancellable: Option<&Cancellable> = None;
          body = request
            .http_body()
            .map(|s| {
              const BUFFER_LEN: usize = 1024;
              let mut result = Vec::new();
              let mut buffer = vec![0; BUFFER_LEN];
              while let Ok(count) = s.read(&mut buffer[..], cancellable) {
                if count == BUFFER_LEN {
                  result.append(&mut buffer);
                  buffer.resize(BUFFER_LEN, 0);
                } else {
                  buffer.truncate(count);
                  result.append(&mut buffer);
                  break;
                }
              }
              result
            })
            .unwrap_or_default();
        }
        #[cfg(not(feature = "linux-body"))]
        {
          body = Vec::new();
        }

        let http_request = match http_request.body(body) {
          Ok(req) => req,
          Err(_) => {
            request.finish_error(&mut gtk::glib::Error::new(
              glib::UriError::Failed,
              "Internal server error: could not create request.",
            ));
            return;
          }
        };

        // GLib's channel API is deprecated in favor of async channels, but keeping it local here
        // avoids adding a new dependency to the vendored Wry fork for this safety fix.
        #[allow(deprecated)]
        let (response_sender, response_receiver) =
          MainContext::channel::<HttpResponse<Cow<'static, [u8]>>>(glib::Priority::default());
        let main_thread_request = request.clone();
        response_receiver.attach(Some(&MainContext::default()), move |http_response| {
          let buffer = http_response.body();
          let input = gtk::gio::MemoryInputStream::from_bytes(&gtk::glib::Bytes::from(buffer));
          let content_type = http_response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|h| h.to_str().ok());

          let response = URISchemeResponse::new(&input, buffer.len() as i64);
          response.set_status(http_response.status().as_u16() as u32, None);
          if let Some(content_type) = content_type {
            response.set_content_type(content_type);
          }

          let headers = MessageHeaders::new(MessageHeadersType::Response);
          for (name, value) in http_response.headers().into_iter() {
            headers.append(name.as_str(), value.to_str().unwrap_or(""));
          }
          response.set_http_headers(headers);
          main_thread_request.finish_with_response(&response);

          glib::ControlFlow::Break
        });
        let responder: Box<dyn FnOnce(HttpResponse<Cow<'static, [u8]>>)> =
          Box::new(move |http_response| {
            let _ = response_sender.send(http_response);
          });

        #[cfg(feature = "tracing")]
        let _span = tracing::info_span!("wry::custom_protocol::call_handler").entered();

        let webview_id = request
          .web_view()
          .and_then(|w| unsafe { w.data::<String>(super::WEBVIEW_ID) })
          .map(|id| unsafe { id.as_ref().clone() })
          .unwrap_or_default();

        handler(&webview_id, http_request, RequestAsyncResponder { responder });
      } else {
        request.finish_error(&mut glib::Error::new(
          glib::FileError::Exist,
          "Could not get uri.",
        ));
      }
    });

    Ok(())
  }

  fn load_uri(&self, webview: WebView, uri: String, headers: Option<http::HeaderMap>) {
    if let Some(headers) = headers {
      let req = URIRequest::builder().uri(&uri).build();

      if let Some(ref mut req_headers) = req.http_headers() {
        for (header, value) in headers.iter() {
          req_headers.append(
            header.to_string().as_str(),
            value.to_str().unwrap_or_default(),
          );
        }
      }

      webview.load_request(&req);
    } else {
      webview.load_uri(&uri);
    }
  }

  fn allows_automation(&self) -> bool {
    self.os.automation
  }

  fn register_automation(&mut self, webview: WebView) {
    if let (true, Some(app_info)) = (self.os.automation, self.os.app_info.take()) {
      self.os.context.connect_automation_started(move |_, auto| {
        let webview = webview.clone();
        auto.set_application_info(&app_info);

        // We do **NOT** support arbitrarily creating new webviews.
        // To support this in the future, we would need a way to specify the
        // default WindowBuilder to use to create the window it will use, and
        // possibly "default" webview attributes. Difficulty comes in for controlling
        // the owned Window that would need to be used.
        //
        // Instead, we just pass the first created webview.
        auto.connect_create_web_view(None, move |_| webview.clone());
      });
    }
  }

  fn register_download_handler(
    &mut self,
    download_started_handler: Option<Box<dyn FnMut(String, &mut PathBuf) -> bool>>,
    download_completed_handler: Option<Rc<dyn Fn(String, Option<PathBuf>, bool) + 'static>>,
  ) {
    let context = &self.os.context;

    let download_started_handler = Rc::new(RefCell::new(download_started_handler));
    let active_downloads = Rc::new(Cell::new(0usize));

    context.connect_download_started(move |_context, download| {
      if active_download_limit_reached(active_downloads.get()) {
        download.cancel();
        return;
      }

      active_downloads.set(active_downloads.get().saturating_add(1));

      let download_started_handler = download_started_handler.clone();
      let active_downloads = active_downloads.clone();
      let failed = Rc::new(RefCell::new(false));
      let slot_released = Rc::new(Cell::new(false));

      download.connect_response_notify({
        let active_downloads = active_downloads.clone();
        let slot_released = slot_released.clone();
        move |download| {
          cancel_download_if_response_too_large(download, &active_downloads, &slot_released);
        }
      });

      download.connect_received_data({
        let active_downloads = active_downloads.clone();
        let slot_released = slot_released.clone();
        move |download, _chunk_len| {
          cancel_download_if_received_data_too_large(download, &active_downloads, &slot_released);
        }
      });

      download.connect_decide_destination({
        let active_downloads = active_downloads.clone();
        let slot_released = slot_released.clone();
        move |download, suggested_filename| {
          if let Some(uri) = download.request().and_then(|req| req.uri()) {
            let uri = uri.to_string();

            if let Some(download_started_handler) = download_started_handler.borrow_mut().as_mut() {
              let mut download_destination =
                dirs::download_dir().unwrap_or_else(|| current_dir().unwrap_or_default());

              let (mut suggested_filename, ext) = suggested_filename
                .split_once('.')
                .map(|(base, ext)| (base, format!(".{ext}")))
                .unwrap_or((suggested_filename, "".to_string()));

              // For `data:` downloads, webkitgtk will suggest to use the raw data as the filename if the dev provided no name,
              // for example `"data:attachment/text,sometext"` will result in `text,sometext` but longer data URLs will
              // result in a cut-off filename, which makes it hard to predict reliably.
              // TODO: If this keeps causing problems, just remove it and use whatever file name webkitgtk suggests.
              if uri.starts_with("data:") {
                if let Some((_, uri_stripped)) = uri.split_once('/') {
                  if let Some((uri_stripped, _)) = uri_stripped.split_once(',') {
                    if suggested_filename.starts_with(&format!("{uri_stripped},")) {
                      suggested_filename = "Unknown";
                    }
                  }
                }
              }

              download_destination.push(format!("{suggested_filename}{ext}"));

              // WebView2 does not overwrite files but appends numbers
              let mut counter = 1;
              while download_destination.exists() {
                download_destination
                  .set_file_name(format!("{suggested_filename} ({counter}){ext}"));
                counter += 1;
              }

              cancel_download_if_response_too_large(download, &active_downloads, &slot_released);
              if slot_released.get() {
                return true;
              }

              if download_started_handler(uri, &mut download_destination) {
                download.set_destination(&download_destination.to_string_lossy());
              } else {
                release_download_slot(&active_downloads, &slot_released);
                download.cancel();
              }
            }
          }
          // TODO: check if we may also need `false`
          true
        }
      });

      download.connect_failed({
        let failed = failed.clone();
        let active_downloads = active_downloads.clone();
        let slot_released = slot_released.clone();
        move |_, _error| {
          *failed.borrow_mut() = true;
          release_download_slot(&active_downloads, &slot_released);
        }
      });

      if let Some(download_completed_handler) = download_completed_handler.clone() {
        download.connect_finished({
          let failed = failed.clone();
          let active_downloads = active_downloads.clone();
          let slot_released = slot_released.clone();
          move |download| {
            release_download_slot(&active_downloads, &slot_released);
            if let Some(uri) = download.request().and_then(|req| req.uri()) {
              let failed = *failed.borrow();
              let uri = uri.to_string();
              download_completed_handler(
                uri,
                (!failed)
                  .then(|| download.destination().map(PathBuf::from))
                  .flatten(),
                !failed,
              )
            }
          }
        });
      }
    });
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn download_active_limit_rejects_fifth_parallel_download() {
    assert!(!active_download_limit_reached(0));
    assert!(!active_download_limit_reached(MAX_ACTIVE_DOWNLOADS - 1));
    assert!(active_download_limit_reached(MAX_ACTIVE_DOWNLOADS));
  }

  #[test]
  fn download_size_limits_allow_unknown_or_small_sizes_only() {
    assert!(!download_content_length_exceeds_limit(0));
    assert!(!download_content_length_exceeds_limit(MAX_DOWNLOAD_BYTES));
    assert!(download_content_length_exceeds_limit(
      MAX_DOWNLOAD_BYTES + 1
    ));
    assert!(!download_received_data_exceeds_limit(MAX_DOWNLOAD_BYTES));
    assert!(download_received_data_exceeds_limit(MAX_DOWNLOAD_BYTES + 1));
  }

  #[test]
  fn release_download_slot_is_idempotent() {
    let active_downloads = Rc::new(Cell::new(1usize));
    let slot_released = Rc::new(Cell::new(false));

    release_download_slot(&active_downloads, &slot_released);
    assert_eq!(active_downloads.get(), 0);

    release_download_slot(&active_downloads, &slot_released);
    assert_eq!(active_downloads.get(), 0);
  }
}
