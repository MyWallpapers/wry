// Copyright 2020-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use once_cell::sync::Lazy;
use windows::{
  core::{HRESULT, HSTRING, PCSTR},
  Win32::{
    Foundation::{FARPROC, HWND, S_OK},
    Graphics::Gdi::{
      GetDC, GetDeviceCaps, MonitorFromWindow, HMONITOR, LOGPIXELSX, MONITOR_DEFAULTTONEAREST,
    },
    System::LibraryLoader::{GetProcAddress, LoadLibraryW},
    UI::{
      HiDpi::{MDT_EFFECTIVE_DPI, MONITOR_DPI_TYPE},
      WindowsAndMessaging::IsProcessDPIAware,
    },
  },
};

/// WebView2 supports non-standard protocols only on Windows 10+, so we have to use a workaround,
/// converting `{protocol}://localhost/abc` to `{http_or_https}://{protocol}.localhost/abc`,
/// and this function tests if the URI starts with `{http_or_https}://{protocol}.`
///
/// See https://github.com/MicrosoftEdge/WebView2Feedback/issues/73
pub(super) fn is_work_around_uri(uri: &str, http_or_https: &str, protocol: &str) -> bool {
  uri
    .strip_prefix(http_or_https)
    .and_then(|rest| rest.strip_prefix("://"))
    .and_then(|rest| rest.strip_prefix(protocol))
    .and_then(|rest| rest.strip_prefix("."))
    .is_some()
}

pub(super) fn apply_uri_work_around(uri: &str, http_or_https: &str, protocol: &str) -> String {
  uri.replace(
    &original_uri_prefix(protocol),
    &work_around_uri_prefix(http_or_https, protocol),
  )
}

pub(super) fn revert_uri_work_around(uri: &str, http_or_https: &str, protocol: &str) -> String {
  uri.replace(
    &work_around_uri_prefix(http_or_https, protocol),
    &original_uri_prefix(protocol),
  )
}

fn original_uri_prefix(protocol: &str) -> String {
  format!("{protocol}://")
}

pub(super) fn work_around_uri_prefix(http_or_https: &str, protocol: &str) -> String {
  format!("{http_or_https}://{protocol}.")
}

fn get_function_impl(library: &str, function: &str) -> FARPROC {
  let library = HSTRING::from(library);
  assert_eq!(function.chars().last(), Some('\0'));

  // Library names we will use are ASCII so we can use the A version to avoid string conversion.
  let module = unsafe { LoadLibraryW(&library) }.unwrap_or_default();
  if module.is_invalid() {
    return None;
  }

  unsafe { GetProcAddress(module, PCSTR::from_raw(function.as_ptr())) }
}

macro_rules! get_function {
  ($lib:expr, $func:ident) => {
    crate::webview2::util::get_function_impl($lib, concat!(stringify!($func), '\0'))
      .map(|f| unsafe { std::mem::transmute::<_, $func>(f) })
  };
}

pub type GetDpiForWindow = unsafe extern "system" fn(hwnd: HWND) -> u32;
pub type GetDpiForMonitor = unsafe extern "system" fn(
  hmonitor: HMONITOR,
  dpi_type: MONITOR_DPI_TYPE,
  dpi_x: *mut u32,
  dpi_y: *mut u32,
) -> HRESULT;

static GET_DPI_FOR_WINDOW: Lazy<Option<GetDpiForWindow>> =
  Lazy::new(|| get_function!("user32.dll", GetDpiForWindow));
static GET_DPI_FOR_MONITOR: Lazy<Option<GetDpiForMonitor>> =
  Lazy::new(|| get_function!("shcore.dll", GetDpiForMonitor));

pub const BASE_DPI: u32 = 96;
pub fn dpi_to_scale_factor(dpi: u32) -> f64 {
  dpi as f64 / BASE_DPI as f64
}

#[inline]
pub(super) fn is_windows_7() -> bool {
  let v = windows_version::OsVersion::current();
  // windows 7 is 6.1
  v.major == 6 && v.minor == 1
}

pub(super) struct UnsafeSend<T>(T);
unsafe impl<T> Send for UnsafeSend<T> {}

impl<T> UnsafeSend<T> {
  pub(super) fn new(value: T) -> Self {
    Self(value)
  }

  pub(super) fn take(self) -> T {
    self.0
  }
}

#[allow(non_snake_case)]
pub unsafe fn hwnd_dpi(hwnd: HWND) -> u32 {
  if let Some(GetDpiForWindow) = *GET_DPI_FOR_WINDOW {
    // We are on Windows 10 Anniversary Update (1607) or later.
    match GetDpiForWindow(hwnd) {
      0 => BASE_DPI, // 0 is returned if hwnd is invalid
      #[allow(clippy::unnecessary_cast)]
      dpi => dpi as u32,
    }
  } else if let Some(GetDpiForMonitor) = *GET_DPI_FOR_MONITOR {
    // We are on Windows 8.1 or later.
    let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    if monitor.is_invalid() {
      return BASE_DPI;
    }

    let mut dpi_x = 0;
    let mut dpi_y = 0;
    #[allow(clippy::unnecessary_cast)]
    if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) == S_OK {
      dpi_x as u32
    } else {
      BASE_DPI
    }
  } else {
    let hdc = GetDC(Some(hwnd));
    if hdc.is_invalid() {
      return BASE_DPI;
    }

    // We are on Vista or later.
    if IsProcessDPIAware().as_bool() {
      // If the process is DPI aware, then scaling must be handled by the application using
      // this DPI value.
      GetDeviceCaps(Some(hdc), LOGPIXELSX) as u32
    } else {
      // If the process is DPI unaware, then scaling is performed by the OS; we thus return
      // 96 (scale factor 1.0) to prevent the window from being re-scaled by both the
      // application and the WM.
      BASE_DPI
    }
  }
}

#[cfg(test)]
mod tests {
  use super::is_work_around_uri;

  #[test]
  fn checks_if_custom_protocol_uri() {
    let scheme = "http";
    let uri = "http://wry.localhost/path/to/page";
    assert!(is_work_around_uri(uri, scheme, "wry"));
    assert!(!is_work_around_uri(uri, scheme, "asset"));
  }
}
