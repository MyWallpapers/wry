use std::{
  cell::RefCell,
  collections::{HashMap, HashSet},
  fmt,
  marker::PhantomData,
  rc::Rc,
  sync::Mutex,
};

use once_cell::sync::Lazy;
use webview2_com::{
  CreateCoreWebView2CompositionControllerCompletedHandler,
  Microsoft::Web::WebView2::Win32::{
    ICoreWebView2CompositionController, ICoreWebView2Controller, ICoreWebView2ControllerOptions3,
    ICoreWebView2Environment, ICoreWebView2Environment10, COREWEBVIEW2_COLOR,
  },
};
use windows::{
  core::{Interface, BOOL},
  Win32::{
    Foundation::{E_POINTER, E_UNEXPECTED, HWND, LPARAM, RECT},
    Graphics::DirectComposition::{
      DCompositionCreateDevice2, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
    },
    System::Threading::GetCurrentThreadId,
    UI::WindowsAndMessaging::{EnumChildWindows, GetWindowThreadProcessId, IsWindow},
  },
};

/// Errors returned by Wry's desktop-composition host.
///
/// The API accepts HWNDs rather than COM interfaces. Wry retains the WebView2
/// composition controller and DirectComposition graph on the WebView owner UI
/// thread, so consumers cannot unbalance COM references or cross STA threads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesktopCompositionError {
  /// No composition controller belongs to the supplied WebView HWND.
  ControllerUnavailable,
  /// The operation was attempted from a thread other than the WebView owner.
  WrongThread { owner: u32, current: u32 },
  /// The supplied presentation HWND does not identify a live window.
  InvalidPresentationWindow,
  /// The startup registry was poisoned before the WebView was constructed.
  RegistryUnavailable,
  /// A live composition controller is already registered for this HWND.
  ControllerAlreadyRegistered,
  /// A WebView2 or DirectComposition transition failed.
  OperationFailed(String),
}

impl fmt::Display for DesktopCompositionError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::ControllerUnavailable => f.write_str("WebView2 composition controller is unavailable"),
      Self::WrongThread { owner, current } => write!(
        f,
        "WebView2 composition controller belongs to UI thread {owner}, not thread {current}"
      ),
      Self::InvalidPresentationWindow => f.write_str("composition presentation HWND is invalid"),
      Self::RegistryUnavailable => {
        f.write_str("desktop-composition startup registry is unavailable")
      }
      Self::ControllerAlreadyRegistered => {
        f.write_str("a desktop-composition controller is already registered for this HWND")
      }
      Self::OperationFailed(message) => f.write_str(message),
    }
  }
}

impl std::error::Error for DesktopCompositionError {}

struct DesktopCompositionState {
  controller: ICoreWebView2CompositionController,
  device: IDCompositionDevice,
  parking_target: IDCompositionTarget,
  visual: IDCompositionVisual,
  presentation_target: Option<IDCompositionTarget>,
  presentation_hwnd: isize,
  owner_thread: u32,
}

std::thread_local! {
  static COMPOSITION_STATES: RefCell<HashMap<isize, DesktopCompositionState>> =
    RefCell::new(HashMap::new());
}

static COMPOSITION_WEBVIEW_IDS: Lazy<Mutex<HashSet<String>>> =
  Lazy::new(|| Mutex::new(HashSet::new()));

/// Opt a Wry WebView id into composition-controller hosting.
///
/// This must run before the matching WebView is constructed. Every other
/// WebView continues to use Wry's standard HWND controller path.
pub fn enable_composition_mode_for_webview_id(
  id: impl AsRef<str>,
) -> Result<(), DesktopCompositionError> {
  COMPOSITION_WEBVIEW_IDS
    .lock()
    .map_err(|_| DesktopCompositionError::RegistryUnavailable)?
    .insert(id.as_ref().to_owned());
  Ok(())
}

pub(crate) fn composition_mode_enabled_for_webview_id(
  id: &str,
) -> Result<bool, DesktopCompositionError> {
  Ok(
    COMPOSITION_WEBVIEW_IDS
      .lock()
      .map_err(|_| DesktopCompositionError::RegistryUnavailable)?
      .contains(id),
  )
}

pub(crate) struct PreparedDesktopComposition {
  controller_hwnd: isize,
  controller: ICoreWebView2Controller,
  state: DesktopCompositionState,
}

impl PreparedDesktopComposition {
  pub(crate) fn controller(&self) -> &ICoreWebView2Controller {
    &self.controller
  }

  pub(crate) fn register(self) -> Result<DesktopCompositionRegistration, DesktopCompositionError> {
    let Self {
      controller_hwnd,
      controller: _,
      state,
    } = self;
    let owner_thread = state.owner_thread;
    ensure_owner_thread(owner_thread, unsafe { GetCurrentThreadId() })?;
    COMPOSITION_STATES.with(|states| {
      let mut states = states.borrow_mut();
      if states.contains_key(&controller_hwnd) {
        return Err(DesktopCompositionError::ControllerAlreadyRegistered);
      }
      states.insert(controller_hwnd, state);
      Ok(DesktopCompositionRegistration {
        controller_hwnd,
        owner_thread,
        _not_send: PhantomData,
      })
    })
  }
}

pub(crate) struct DesktopCompositionRegistration {
  controller_hwnd: isize,
  owner_thread: u32,
  _not_send: PhantomData<Rc<()>>,
}

impl Drop for DesktopCompositionRegistration {
  fn drop(&mut self) {
    if self.owner_thread != unsafe { GetCurrentThreadId() } {
      return;
    }
    COMPOSITION_STATES.with(|states| {
      states.borrow_mut().remove(&self.controller_hwnd);
    });
  }
}

fn as_wry_error(error: windows::core::Error) -> crate::Error {
  crate::Error::WebView2Error(webview2_com::Error::WindowsError(error))
}

pub(crate) fn prepare_desktop_composition(
  controller_hwnd: HWND,
  environment: &ICoreWebView2Environment,
  incognito: bool,
  background_color: Option<(u8, u8, u8, u8)>,
) -> crate::Result<PreparedDesktopComposition> {
  let environment: ICoreWebView2Environment10 = environment.cast().map_err(as_wry_error)?;
  let controller_options =
    unsafe { environment.CreateCoreWebView2ControllerOptions() }.map_err(as_wry_error)?;

  if let Some((red, green, blue, mut alpha)) = background_color {
    let options: ICoreWebView2ControllerOptions3 =
      controller_options.cast().map_err(as_wry_error)?;
    if alpha != 0 {
      alpha = 255;
    }
    unsafe {
      options.SetDefaultBackgroundColor(COREWEBVIEW2_COLOR {
        R: red,
        G: green,
        B: blue,
        A: alpha,
      })
    }
    .map_err(as_wry_error)?;
  }
  unsafe { controller_options.SetIsInPrivateModeEnabled(incognito) }.map_err(as_wry_error)?;

  let (sender, receiver) = std::sync::mpsc::channel();
  let handler = CreateCoreWebView2CompositionControllerCompletedHandler::create(Box::new(
    move |error_code, controller| {
      error_code?;
      sender
        .send(controller.ok_or_else(|| windows::core::Error::from(E_POINTER)))
        .map_err(|_| windows::core::Error::from(E_UNEXPECTED))
    },
  ));
  unsafe {
    environment.CreateCoreWebView2CompositionControllerWithOptions(
      controller_hwnd,
      &controller_options,
      &handler,
    )
  }
  .map_err(as_wry_error)?;

  let composition_controller = webview2_com::wait_with_pump(receiver)?.map_err(as_wry_error)?;
  let controller: ICoreWebView2Controller = composition_controller.cast().map_err(as_wry_error)?;

  let device: IDCompositionDevice =
    unsafe { DCompositionCreateDevice2(None) }.map_err(as_wry_error)?;
  let parking_target =
    unsafe { device.CreateTargetForHwnd(controller_hwnd, true) }.map_err(as_wry_error)?;
  let visual = unsafe { device.CreateVisual() }.map_err(as_wry_error)?;
  unsafe { parking_target.SetRoot(&visual) }.map_err(as_wry_error)?;
  let visual_unknown: windows::core::IUnknown = visual.cast().map_err(as_wry_error)?;
  unsafe { composition_controller.SetRootVisualTarget(&visual_unknown) }.map_err(as_wry_error)?;
  unsafe { device.Commit() }.map_err(as_wry_error)?;

  Ok(PreparedDesktopComposition {
    controller_hwnd: controller_hwnd.0 as isize,
    controller,
    state: DesktopCompositionState {
      controller: composition_controller,
      device,
      parking_target,
      visual,
      presentation_target: None,
      presentation_hwnd: 0,
      owner_thread: unsafe { GetCurrentThreadId() },
    },
  })
}

#[derive(Default)]
struct ControllerLookup {
  hwnd: isize,
}

unsafe extern "system" fn find_controller_child(hwnd: HWND, parameter: LPARAM) -> BOOL {
  if parameter.0 == 0 {
    return BOOL(0);
  }
  let lookup = &mut *(parameter.0 as *mut ControllerLookup);
  let found = COMPOSITION_STATES.with(|states| states.borrow().contains_key(&(hwnd.0 as isize)));
  if found {
    lookup.hwnd = hwnd.0 as isize;
    return BOOL(0);
  }
  BOOL(1)
}

fn controller_hwnd_for_hwnd(hwnd: HWND) -> Result<isize, DesktopCompositionError> {
  if hwnd.is_invalid() || !unsafe { IsWindow(Some(hwnd)).as_bool() } {
    return Err(DesktopCompositionError::ControllerUnavailable);
  }
  let owner = unsafe { GetWindowThreadProcessId(hwnd, None) };
  if owner == 0 {
    return Err(DesktopCompositionError::ControllerUnavailable);
  }
  ensure_owner_thread(owner, unsafe { GetCurrentThreadId() })?;

  let controller_hwnd = hwnd.0 as isize;
  if COMPOSITION_STATES.with(|states| states.borrow().contains_key(&controller_hwnd)) {
    return Ok(controller_hwnd);
  }

  let mut lookup = ControllerLookup::default();
  unsafe {
    let _ = EnumChildWindows(
      Some(hwnd),
      Some(find_controller_child),
      LPARAM(&mut lookup as *mut _ as isize),
    );
  }
  (lookup.hwnd != 0)
    .then_some(lookup.hwnd)
    .ok_or(DesktopCompositionError::ControllerUnavailable)
}

fn ensure_owner_thread(owner: u32, current: u32) -> Result<(), DesktopCompositionError> {
  if owner == current {
    Ok(())
  } else {
    Err(DesktopCompositionError::WrongThread { owner, current })
  }
}

fn with_state<T>(
  controller_hwnd: HWND,
  operation: impl FnOnce(&DesktopCompositionState) -> Result<T, DesktopCompositionError>,
) -> Result<T, DesktopCompositionError> {
  let controller_hwnd = controller_hwnd_for_hwnd(controller_hwnd)?;
  COMPOSITION_STATES.with(|states| {
    let states = states.borrow();
    let state = states
      .get(&controller_hwnd)
      .ok_or(DesktopCompositionError::ControllerUnavailable)?;
    ensure_owner_thread(state.owner_thread, unsafe { GetCurrentThreadId() })?;
    operation(state)
  })
}

fn with_state_mut<T>(
  controller_hwnd: HWND,
  operation: impl FnOnce(&mut DesktopCompositionState) -> Result<T, DesktopCompositionError>,
) -> Result<T, DesktopCompositionError> {
  let controller_hwnd = controller_hwnd_for_hwnd(controller_hwnd)?;
  COMPOSITION_STATES.with(|states| {
    let mut states = states.borrow_mut();
    let state = states
      .get_mut(&controller_hwnd)
      .ok_or(DesktopCompositionError::ControllerUnavailable)?;
    ensure_owner_thread(state.owner_thread, unsafe { GetCurrentThreadId() })?;
    operation(state)
  })
}

unsafe fn restore_visual(
  state: &DesktopCompositionState,
  attempted_target: &IDCompositionTarget,
  previous_target: &IDCompositionTarget,
) -> windows::core::Result<()> {
  attempted_target.SetRoot(None::<&IDCompositionVisual>)?;
  previous_target.SetRoot(&state.visual)?;
  state.device.Commit()
}

fn transition_error(
  operation: &str,
  original: windows::core::Error,
  rollback: windows::core::Result<()>,
) -> DesktopCompositionError {
  match rollback {
    Ok(()) => DesktopCompositionError::OperationFailed(format!("{operation}: {original}")),
    Err(rollback) => DesktopCompositionError::OperationFailed(format!(
      "{operation}: {original}; restoring the last committed target also failed: {rollback}"
    )),
  }
}

/// Verify that the composition controller exists on the caller's UI thread.
pub fn desktop_composition_ready_for_hwnd(
  controller_hwnd: HWND,
) -> Result<(), DesktopCompositionError> {
  with_state(controller_hwnd, |_| Ok(()))
}

/// Move the existing WebView2 visual to a process-owned presentation HWND.
pub fn retarget_desktop_composition_for_hwnd(
  controller_hwnd: HWND,
  presentation_hwnd: HWND,
) -> Result<(), DesktopCompositionError> {
  if presentation_hwnd.is_invalid() || !unsafe { IsWindow(Some(presentation_hwnd)).as_bool() } {
    return Err(DesktopCompositionError::InvalidPresentationWindow);
  }

  with_state_mut(controller_hwnd, |state| unsafe {
    if state.presentation_hwnd == presentation_hwnd.0 as isize
      && state.presentation_target.is_some()
    {
      return Ok(());
    }

    let target = state
      .device
      .CreateTargetForHwnd(presentation_hwnd, true)
      .map_err(|error| {
        DesktopCompositionError::OperationFailed(format!("CreateTargetForHwnd failed: {error}"))
      })?;
    let previous_target = state
      .presentation_target
      .clone()
      .unwrap_or_else(|| state.parking_target.clone());
    previous_target
      .SetRoot(None::<&IDCompositionVisual>)
      .map_err(|error| {
        DesktopCompositionError::OperationFailed(format!(
          "clearing the previous composition target failed: {error}"
        ))
      })?;

    if let Err(error) = target
      .SetRoot(&state.visual)
      .and_then(|()| state.device.Commit())
    {
      return Err(transition_error(
        "attaching the presentation target failed",
        error,
        restore_visual(state, &target, &previous_target),
      ));
    }

    state.presentation_target = Some(target);
    state.presentation_hwnd = presentation_hwnd.0 as isize;
    Ok(())
  })
}

/// Return the WebView2 visual to its private parking target.
///
/// The operation is idempotent and updates logical state only after a
/// successful DirectComposition commit.
pub fn detach_desktop_composition_for_hwnd(
  controller_hwnd: HWND,
) -> Result<(), DesktopCompositionError> {
  with_state_mut(controller_hwnd, |state| unsafe {
    let Some(previous_target) = state.presentation_target.clone() else {
      state.presentation_hwnd = 0;
      return Ok(());
    };

    previous_target
      .SetRoot(None::<&IDCompositionVisual>)
      .map_err(|error| {
        DesktopCompositionError::OperationFailed(format!(
          "detaching the presentation target failed: {error}"
        ))
      })?;
    if let Err(error) = state
      .parking_target
      .SetRoot(&state.visual)
      .and_then(|()| state.device.Commit())
    {
      return Err(transition_error(
        "restoring the parking target failed",
        error,
        restore_visual(state, &state.parking_target, &previous_target),
      ));
    }

    state.presentation_target = None;
    state.presentation_hwnd = 0;
    Ok(())
  })
}

/// Report whether the visual is committed to the supplied presentation HWND.
pub fn desktop_composition_is_attached_to_hwnd(
  controller_hwnd: HWND,
  presentation_hwnd: HWND,
) -> Result<bool, DesktopCompositionError> {
  with_state(controller_hwnd, |state| {
    Ok(
      state.presentation_hwnd == presentation_hwnd.0 as isize
        && state.presentation_target.is_some(),
    )
  })
}

/// Resize the WebView2 composition surface in physical pixels.
pub fn set_desktop_composition_bounds_for_hwnd(
  controller_hwnd: HWND,
  width: i32,
  height: i32,
) -> Result<(), DesktopCompositionError> {
  if width <= 0 || height <= 0 {
    return Err(DesktopCompositionError::OperationFailed(
      "WebView2 composition bounds must be positive".to_string(),
    ));
  }
  with_state(controller_hwnd, |state| unsafe {
    let controller: ICoreWebView2Controller = state.controller.cast().map_err(|error| {
      DesktopCompositionError::OperationFailed(format!(
        "querying ICoreWebView2Controller failed: {error}"
      ))
    })?;
    controller
      .SetBounds(RECT {
        left: 0,
        top: 0,
        right: width,
        bottom: height,
      })
      .map_err(|error| {
        DesktopCompositionError::OperationFailed(format!("SetBounds failed: {error}"))
      })?;
    controller
      .NotifyParentWindowPositionChanged()
      .map_err(|error| {
        DesktopCompositionError::OperationFailed(format!(
          "NotifyParentWindowPositionChanged failed: {error}"
        ))
      })
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn desktop_composition_error_is_explicit() {
    assert!(DesktopCompositionError::ControllerUnavailable
      .to_string()
      .contains("unavailable"));
  }

  #[test]
  fn wrong_owner_thread_is_rejected() {
    assert_eq!(
      ensure_owner_thread(41, 42),
      Err(DesktopCompositionError::WrongThread {
        owner: 41,
        current: 42,
      })
    );
  }

  #[test]
  fn composition_mode_is_opt_in_per_webview_id() {
    let id = "mywallpaper-test-composition-opt-in";
    assert_eq!(composition_mode_enabled_for_webview_id(id), Ok(false));
    assert_eq!(enable_composition_mode_for_webview_id(id), Ok(()));
    assert_eq!(composition_mode_enabled_for_webview_id(id), Ok(true));
  }

  #[repr(C)]
  struct RefCountedUnknown {
    vtable: *const windows::core::IUnknown_Vtbl,
    references: std::sync::atomic::AtomicU32,
    add_refs: std::sync::atomic::AtomicU32,
    releases: std::sync::atomic::AtomicU32,
  }

  unsafe extern "system" fn query_interface(
    _this: *mut std::ffi::c_void,
    _iid: *const windows::core::GUID,
    interface: *mut *mut std::ffi::c_void,
  ) -> windows::core::HRESULT {
    *interface = std::ptr::null_mut();
    windows::core::HRESULT(0x8000_4002_u32 as i32)
  }

  unsafe extern "system" fn add_ref(this: *mut std::ffi::c_void) -> u32 {
    let tracked = &*(this as *const RefCountedUnknown);
    tracked
      .add_refs
      .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    tracked
      .references
      .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
      + 1
  }

  unsafe extern "system" fn release(this: *mut std::ffi::c_void) -> u32 {
    let tracked = &*(this as *const RefCountedUnknown);
    tracked
      .releases
      .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    tracked
      .references
      .fetch_sub(1, std::sync::atomic::Ordering::SeqCst)
      - 1
  }

  static REF_COUNTED_UNKNOWN_VTABLE: windows::core::IUnknown_Vtbl = windows::core::IUnknown_Vtbl {
    QueryInterface: query_interface,
    AddRef: add_ref,
    Release: release,
  };

  #[test]
  fn composition_controller_clone_balances_addref_and_release() {
    unsafe {
      let mut tracked = Box::new(RefCountedUnknown {
        vtable: &REF_COUNTED_UNKNOWN_VTABLE,
        references: std::sync::atomic::AtomicU32::new(1),
        add_refs: std::sync::atomic::AtomicU32::new(1),
        releases: std::sync::atomic::AtomicU32::new(0),
      });
      let original =
        windows::core::IUnknown::from_raw((&mut *tracked as *mut RefCountedUnknown).cast());
      let retained = original.clone();
      drop(retained);
      drop(original);

      assert_eq!(
        tracked.add_refs.load(std::sync::atomic::Ordering::SeqCst),
        tracked.releases.load(std::sync::atomic::Ordering::SeqCst),
      );
      assert_eq!(
        tracked.references.load(std::sync::atomic::Ordering::SeqCst),
        0,
      );
    }
  }
}
