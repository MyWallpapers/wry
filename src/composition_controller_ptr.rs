#[cfg(target_os = "windows")]
use webview2_com::Microsoft::Web::WebView2::Win32::{
  ICoreWebView2CompositionController, ICoreWebView2Controller, COREWEBVIEW2_MOUSE_EVENT_KIND,
  COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS,
};
#[cfg(target_os = "windows")]
use windows::core::w;
#[cfg(target_os = "windows")]
use windows::core::Interface;
#[cfg(target_os = "windows")]
use windows::Win32::Foundation::{HANDLE, HWND, POINT, RECT};
#[cfg(target_os = "windows")]
use windows::Win32::UI::WindowsAndMessaging::{
  GetPropW, RemovePropW, SetPropW, SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER,
};

#[cfg(target_os = "windows")]
const COMP_CONTROLLER_PROP_NAME: windows::core::PCWSTR =
  w!("MyWallpaper.Wry.WebView2CompositionController");

/// Returns the raw COM pointer scoped to the HWND that owns the composition
/// controller. Returns 0 if no controller is published for that window.
#[cfg(target_os = "windows")]
pub fn get_composition_controller_ptr_for_hwnd_raw(controller_hwnd: isize) -> isize {
  if controller_hwnd == 0 {
    return 0;
  }

  let hwnd = HWND(controller_hwnd as *mut _);
  if hwnd.is_invalid() {
    return 0;
  }

  // SAFETY: HWND is process-local and the property contains a raw pointer-sized
  // value published by this crate for that exact window.
  unsafe { GetPropW(hwnd, COMP_CONTROLLER_PROP_NAME).0 as isize }
}

#[cfg(target_os = "windows")]
pub(crate) fn store_composition_controller_ptr_for_hwnd_raw(
  controller_ptr: isize,
  controller_hwnd: isize,
) {
  if controller_ptr == 0 || controller_hwnd == 0 {
    return;
  }

  let hwnd = HWND(controller_hwnd as *mut _);
  if hwnd.is_invalid() {
    return;
  }

  // SAFETY: The property key is crate-owned and the value is a process-local
  // raw COM pointer tied to this HWND for the lifetime of the controller.
  unsafe {
    let _ = SetPropW(
      hwnd,
      COMP_CONTROLLER_PROP_NAME,
      Some(HANDLE(controller_ptr as *mut _)),
    );
  }
}

#[cfg(target_os = "windows")]
pub(crate) fn clear_composition_controller_ptr_for_hwnd_raw(
  controller_hwnd: isize,
  expected_ptr: isize,
) {
  if controller_hwnd == 0 || expected_ptr == 0 {
    return;
  }

  let hwnd = HWND(controller_hwnd as *mut _);
  if hwnd.is_invalid() {
    return;
  }

  // SAFETY: We only remove the property when it still matches the controller
  // being dropped, so a newer controller on the same HWND is never clobbered.
  unsafe {
    if GetPropW(hwnd, COMP_CONTROLLER_PROP_NAME).0 as isize == expected_ptr {
      let _ = RemovePropW(hwnd, COMP_CONTROLLER_PROP_NAME);
    }
  }
}

/// Retain one COM reference on a raw WebView2 composition-controller pointer.
#[cfg(target_os = "windows")]
pub fn retain_composition_controller_ptr_raw(comp_ptr: isize) -> std::result::Result<isize, String> {
  if comp_ptr == 0 {
    return Err("Null composition controller".to_string());
  }

  // SAFETY: The caller passes a process-local WebView2 composition controller
  // pointer previously published by this crate. We borrow it without releasing
  // ownership, then clone one COM reference for the returned retained handle.
  let borrowed = unsafe {
    std::mem::ManuallyDrop::new(ICoreWebView2CompositionController::from_raw(
      comp_ptr as *mut std::ffi::c_void,
    ))
  };
  let retained = borrowed.clone();
  let retained_ptr = retained.as_raw() as isize;
  std::mem::forget(retained);
  Ok(retained_ptr)
}

/// Release a COM reference previously retained via
/// `retain_composition_controller_ptr_raw`.
#[cfg(target_os = "windows")]
pub fn release_composition_controller_ptr_raw(comp_ptr: isize) {
  if comp_ptr == 0 {
    return;
  }

  // SAFETY: The caller passes exactly one retained COM reference obtained from
  // `retain_composition_controller_ptr_raw`, so reconstructing and dropping the
  // interface releases that owned reference once.
  unsafe {
    drop(ICoreWebView2CompositionController::from_raw(
      comp_ptr as *mut std::ffi::c_void,
    ));
  }
}

/// Send a mouse input event via the WebView2 composition controller.
///
/// This is a free function that takes a raw COM pointer, allowing it to be
/// called from any thread with plain integer types.
///
/// # Safety
/// `comp_ptr` must be a valid `ICoreWebView2CompositionController` COM pointer.
/// The pointer must remain valid for the duration of the call.
#[cfg(target_os = "windows")]
pub unsafe fn send_mouse_input_raw(
  comp_ptr: isize,
  event_kind: i32,
  virtual_keys: i32,
  mouse_data: u32,
  x: i32,
  y: i32,
) -> std::result::Result<(), String> {
  if comp_ptr == 0 {
    return Err("Null composition controller".to_string());
  }

  let comp = std::mem::ManuallyDrop::new(ICoreWebView2CompositionController::from_raw(
    comp_ptr as *mut std::ffi::c_void,
  ));

  comp
    .SendMouseInput(
      COREWEBVIEW2_MOUSE_EVENT_KIND(event_kind),
      COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS(virtual_keys),
      mouse_data,
      POINT { x, y },
    )
    .map_err(|e| format!("SendMouseInput failed: {}", e))?;

  Ok(())
}

/// Set the WebView2 controller bounds directly via a raw composition
/// controller pointer.
///
/// # Safety
/// `comp_ptr` must be a valid `ICoreWebView2CompositionController` COM pointer.
#[cfg(target_os = "windows")]
pub unsafe fn set_controller_bounds_raw(
  comp_ptr: isize,
  width: i32,
  height: i32,
) -> std::result::Result<(), String> {
  if comp_ptr == 0 {
    return Err("Null composition controller".to_string());
  }

  let comp = std::mem::ManuallyDrop::new(ICoreWebView2CompositionController::from_raw(
    comp_ptr as *mut std::ffi::c_void,
  ));
  let controller: ICoreWebView2Controller = comp
    .cast()
    .map_err(|e| format!("QI for ICoreWebView2Controller failed: {}", e))?;

  let mut container = HWND::default();
  if controller.ParentWindow(&mut container).is_ok() && !container.is_invalid() {
    let _ = SetWindowPos(
      container,
      None,
      0,
      0,
      width,
      height,
      SWP_NOACTIVATE | SWP_NOZORDER,
    );
  }

  controller
    .SetBounds(RECT {
      left: 0,
      top: 0,
      right: width,
      bottom: height,
    })
    .map_err(|e| format!("SetBounds failed: {}", e))?;

  let _ = controller.NotifyParentWindowPositionChanged();
  Ok(())
}

/// Re-assert WebView2 visibility via the composition controller.
///
/// # Safety
/// `comp_ptr` must be a valid `ICoreWebView2CompositionController` COM pointer.
#[cfg(target_os = "windows")]
pub unsafe fn set_is_visible_raw(comp_ptr: isize) -> std::result::Result<(), String> {
  if comp_ptr == 0 {
    return Err("Null composition controller".to_string());
  }

  let comp = std::mem::ManuallyDrop::new(ICoreWebView2CompositionController::from_raw(
    comp_ptr as *mut std::ffi::c_void,
  ));
  let controller: ICoreWebView2Controller = comp
    .cast()
    .map_err(|e| format!("QI for ICoreWebView2Controller failed: {}", e))?;

  controller
    .SetIsVisible(true)
    .map_err(|e| format!("SetIsVisible failed: {}", e))?;

  Ok(())
}

#[cfg(test)]
mod tests {
  #[test]
  fn composition_controller_lookup_rejects_null_hwnd() {
    #[cfg(target_os = "windows")]
    assert_eq!(super::get_composition_controller_ptr_for_hwnd_raw(0), 0);
  }
}
