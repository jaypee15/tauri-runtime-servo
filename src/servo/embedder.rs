// Copyright 2020-2026 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::{
  cell::{Cell, RefCell},
  collections::HashMap,
  rc::Rc,
  sync::Arc,
};

use euclid::{
  default::{Point2D, Rect as EuclidRect, Size2D},
  Scale,
};
use keyboard_types::{
  Code, CompositionEvent, CompositionState, Key, KeyState, Location, Modifiers, NamedKey,
};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use servo::{
  protocol_handler::{
    DoneChannel, FetchContext, HttpStatus, NetworkError, ProtocolHandler, ProtocolRegistry,
    Request as ServoRequest, ResourceFetchTiming, Response as ServoResponse, ResponseBody,
  },
  DeviceIntRect, DevicePoint, EventLoopWaker, ImeEvent, InputEvent, KeyboardEvent, LoadStatus, MouseButtonAction,
  MouseButtonEvent, MouseLeftViewportEvent, MouseMoveEvent, NavigationRequest,
  OffscreenRenderingContext, Preferences, RenderingContext, Servo, ServoBuilder, WebResourceLoad,
  WebResourceResponse,
  Theme as ServoTheme, TouchEvent, TouchEventType, TouchId, TouchPointerType, UrlRequest,
  UserContentManager, UserScript, WebView as ServoWebView, WebViewBuilder as ServoWebViewBuilder,
  WebViewDelegate, WheelDelta, WheelEvent, WheelMode, WindowRenderingContext,
};
use super::compositor::{Bounds, ChildId, Compositor, Overlay};
use super::engine_registry::EngineRegistry;
use super::nav_gate::{NavAction, NavGate};
use tao::{
  dpi::{PhysicalPosition, PhysicalSize},
  event::{ElementState, MouseButton as TaoMouseButton, MouseScrollDelta, TouchPhase, WindowEvent},
  event_loop::{ControlFlow, EventLoopProxy},
  keyboard::{Key as TaoKey, KeyCode as TaoKeyCode, KeyLocation, ModifiersState},
  window::Window,
};
use url::Url;

use crate::{
  InitializationScript, PageLoadEvent, Rect, RequestAsyncResponder, ServoError as Error,
  ServoResult as Result, WebViewId,
};

type CustomProtocolHandler =
  Box<dyn Fn(WebViewId, http::Request<Vec<u8>>, RequestAsyncResponder) + Send + Sync>;

const IPC_MESSAGE_PREFIX: &str = "__SERVO_IPC__:";
const IPC_BRIDGE_SCRIPT: &str = r#"
  Object.defineProperty(window, 'ipc', {
    value: Object.freeze({
      postMessage: function(message) {
        // The payload goes in as a `%s` argument, never as the format string: the console
        // treats `%c`, `%s`, `%%` ... in the first argument as directives and would corrupt
        // anything containing them (the invoke key, percent-encoded URLs).
        console.debug('__SERVO_IPC__:%s', String(message));
      }
    })
  });
"#;

/// The URI an IPC message is attributed to: the page's URL when it is a valid absolute URI that
/// Tauri can parse as a URL. Anything else (`data:`, `blob:`, `about:blank`, no URL) is attributed
/// to a fixed remote origin, so the message is evaluated as a remote page's and denied. A relative
/// fallback such as `/` makes Tauri's `Url::parse(..).expect(..)` panic on the main thread.
/// The URI an IPC message is attributed to: the page's URL when it is a valid absolute URI that
/// Tauri can parse as a URL. Anything else (`data:`, `blob:`, `about:blank`, no URL) is attributed
/// to a fixed remote origin, so the message is evaluated as a remote page's and denied. A relative
/// fallback such as `/` makes Tauri's `Url::parse(..).expect(..)` panic on the main thread.
fn ipc_request_uri(page_url: Option<&str>) -> http::Uri {
  const FALLBACK: &str = "http://invalid.invalid/";
  page_url
    .filter(|url| url::Url::parse(url).is_ok())
    .and_then(|url| url.parse::<http::Uri>().ok())
    .filter(|uri| uri.scheme().is_some() && uri.authority().is_some())
    .unwrap_or_else(|| FALLBACK.parse().expect("fallback URI is valid"))
}

fn ipc_message_body(message: &str) -> Option<&str> {
  message.strip_prefix(IPC_MESSAGE_PREFIX)
}

/// (runtime webview id, handler) for one custom-protocol scheme of one webview.
type ProtocolRoute = (String, std::sync::Arc<CustomProtocolHandler>);

/// Custom-protocol handlers per webview. The Servo engine is shared, so every scheme is registered
/// with it once as a `RoutedProtocol`, and each request is routed to the handler of the webview
/// that issued it (`ServoRequest::target_webview_id`).
static ROUTES: std::sync::LazyLock<
  std::sync::Mutex<super::protocol_routes::ProtocolRoutes<servo::WebViewId, ProtocolRoute>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(super::protocol_routes::ProtocolRoutes::new()));

struct RoutedProtocol {
  scheme: String,
}

impl ProtocolHandler for RoutedProtocol {
  fn load<'a>(
    &'a self,
    request: &'a mut ServoRequest,
    _done_chan: &mut DoneChannel,
    _context: &FetchContext,
  ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ServoResponse> + Send + 'a>> {
    let route = ROUTES
      .lock()
      .ok()
      .and_then(|routes| routes.resolve(request.target_webview_id.as_ref(), &self.scheme));
    let Some((webview_id, handler)) = route else {
      return Box::pin(std::future::ready(ServoResponse::network_error(
        NetworkError::ResourceLoadError(format!(
          "no webview registered for custom protocol {}",
          self.scheme
        )),
      )));
    };
    let url = request.current_url();
    let timing_type = request.timing_type();
    let method = request.method.clone();
    let headers = request.headers.clone();
    let request = http::Request::builder()
      .method(method)
      .uri(url.as_str())
      .body(Vec::new());

    let Ok(mut request) = request else {
      return Box::pin(std::future::ready(ServoResponse::network_error(
        NetworkError::ResourceLoadError(format!("invalid custom protocol URL: {url}")),
      )));
    };
    *request.headers_mut() = headers;

    let (sender, receiver) = futures_channel::oneshot::channel();
    (*handler)(
      &webview_id,
      request,
      RequestAsyncResponder {
        responder: Box::new(move |response| {
          let _ = sender.send(response);
        }),
      },
    );

    Box::pin(async move {
      match receiver.await {
        Ok(response) => {
          let (parts, body) = response.into_parts();
          let mut response = ServoResponse::new(url, ResourceFetchTiming::new(timing_type));
          response.status = HttpStatus::new_raw(
            parts.status.as_u16(),
            parts
              .status
              .canonical_reason()
              .unwrap_or_default()
              .as_bytes()
              .to_vec(),
          );
          response.headers = parts.headers;
          *response.body.lock() = ResponseBody::Done(body.into_owned());
          response
        }
        Err(_) => ServoResponse::network_error(NetworkError::ResourceLoadError(
          "custom protocol response channel closed".into(),
        )),
      }
    })
  }

  fn is_fetchable(&self) -> bool {
    true
  }

  fn is_secure(&self) -> bool {
    true
  }
}

#[derive(Clone)]
struct EmbedderWaker(Arc<dyn Fn() + Send + Sync>);

impl EmbedderWaker {
  fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
    Self(Arc::new(wake))
  }
}

impl EventLoopWaker for EmbedderWaker {
  fn clone_box(&self) -> Box<dyn EventLoopWaker> {
    Box::new(self.clone())
  }

  fn wake(&self) {
    (self.0)();
  }
}

struct Delegate {
  window: Option<Rc<Window>>,
  waker: EmbedderWaker,
  /// A navigation requested while the initial (about:blank) load is still in flight can be lost,
  /// so it is deferred until that load completes (or a timeout passes).
  nav_gate: RefCell<NavGate>,
  /// This webview's Tauri label, reported to the app's resource filter.
  label: String,
  frame_ready: Cell<bool>,
  closed: Cell<bool>,
  ipc_handler: Option<Box<dyn Fn(http::Request<String>)>>,
  navigation_handler: Option<Box<dyn Fn(String) -> bool>>,
  document_title_changed_handler: Option<Box<dyn Fn(String)>>,
  on_page_load_handler: Option<Box<dyn Fn(PageLoadEvent, String)>>,
}

impl Delegate {
  fn request_repaint(&self) {
    self.frame_ready.set(true);
    if let Some(window) = &self.window {
      window.request_redraw();
    } else {
      self.waker.wake();
    }
  }
}

impl WebViewDelegate for Delegate {
  fn show_embedder_control(&self, webview: ServoWebView, control: servo::EmbedderControl) {
    use servo::EmbedderControl as C;
    let control_id = control.id();
    let px = |r: servo::DeviceIntRect| (r.min.x, r.min.y, r.width().max(0) as u32, r.height().max(0) as u32);
    match control {
      C::SelectElement(select) => {
        let popup = SelectPopup {
          label: self.label.clone(),
          rect: px(select.position()),
          items: flatten_select_options(select.options()),
          selected: select.selected_options(),
          multiple: select.allow_select_multiple(),
        };
        // A newer picker replaces an older one; dropping the old one resolves it unchanged.
        PENDING_SELECT.with(|p| *p.borrow_mut() = Some(select));
        notify_select(SelectEvent::Show(popup));
      }
      C::ColorPicker(picker) => {
        let popup = ColorPopup {
          label: self.label.clone(),
          rect: px(picker.position()),
          current: picker.current_color().map(|c| (c.red, c.green, c.blue)),
        };
        PENDING_COLOR.with(|p| *p.borrow_mut() = Some((picker, self.label.clone())));
        notify_control(ControlEvent::Color(popup));
      }
      C::FilePicker(picker) => {
        let popup = FilePopup {
          label: self.label.clone(),
          patterns: picker.filter_patterns().iter().map(|f| f.0.clone()).collect(),
          multiple: picker.allow_select_multiple(),
          current: picker.current_paths().to_vec(),
        };
        PENDING_FILE.with(|p| *p.borrow_mut() = Some(picker));
        notify_control(ControlEvent::File(popup));
      }
      C::SimpleDialog(dialog) => {
        let (kind, default) = match &dialog {
          servo::SimpleDialog::Alert(_) => (DialogKind::Alert, String::new()),
          servo::SimpleDialog::Confirm(_) => (DialogKind::Confirm, String::new()),
          servo::SimpleDialog::Prompt(p) => (DialogKind::Prompt, p.current_value().to_string()),
        };
        let popup = DialogPopup { label: self.label.clone(), kind, message: dialog.message().to_string(), default };
        PENDING_DIALOG.with(|p| *p.borrow_mut() = Some((dialog, control_id)));
        notify_control(ControlEvent::Dialog(popup));
      }
      C::InputMethod(ime) => {
        // Only the date and time kinds get a picker; text fields keep the engine's default.
        let kind = match ime.input_method_type() {
          servo::InputMethodType::Date => "date",
          servo::InputMethodType::DatetimeLocal => "datetime-local",
          servo::InputMethodType::Month => "month",
          servo::InputMethodType::Time => "time",
          servo::InputMethodType::Week => "week",
          _ => return,
        };
        let popup = DatePopup { label: self.label.clone(), rect: px(ime.position()), kind, value: ime.text() };
        // Remember which input asked: the picker is drawn by another page, and the page's focus may
        // move before the answer comes back.
        webview.evaluate_javascript("window.__bifrostDateTarget=document.activeElement", |_| {});
        PENDING_DATE.with(|p| *p.borrow_mut() = Some((self.label.clone(), control_id)));
        notify_control(ControlEvent::Date(popup));
      }
      C::ContextMenu(menu) => {
        let items = menu
          .items()
          .iter()
          .map(|item| match item {
            servo::ContextMenuItem::Item { label, enabled, .. } => {
              MenuEntry::Item { label: label.replace(" in New View", " in New Tab"), enabled: *enabled }
            }
            servo::ContextMenuItem::Separator => MenuEntry::Separator,
          })
          .collect();
        let (x, y) = LAST_CURSOR.with(|c| c.get());
        let popup = MenuPopup { label: self.label.clone(), x, y, items };
        PENDING_MENU.with(|p| *p.borrow_mut() = Some(menu));
        notify_control(ControlEvent::Menu(popup));
      }
    }
  }

  fn hide_embedder_control(&self, _webview: ServoWebView, control_id: servo::EmbedderControlId) {
    let select = PENDING_SELECT.with(|p| {
      let mut slot = p.borrow_mut();
      if slot.as_ref().is_some_and(|s| s.id() == control_id) { slot.take(); true } else { false }
    });
    if select {
      notify_select(SelectEvent::Hide);
    }
    let color = PENDING_COLOR.with(|p| {
      let mut slot = p.borrow_mut();
      if slot.as_ref().is_some_and(|(c, _)| c.id() == control_id) { slot.take(); true } else { false }
    });
    let file = PENDING_FILE.with(|p| {
      let mut slot = p.borrow_mut();
      if slot.as_ref().is_some_and(|f| f.id() == control_id) { slot.take(); true } else { false }
    });
    // A dialog resolved by the page (navigation) closes its box too. Match by id: the engine can
    // report the previous dialog hidden after the next one has already appeared.
    let dialog = PENDING_DIALOG.with(|p| {
      let mut slot = p.borrow_mut();
      if slot.as_ref().is_some_and(|(_, id)| *id == control_id) { slot.take(); true } else { false }
    });
    let date = PENDING_DATE.with(|p| {
      let mut slot = p.borrow_mut();
      if slot.as_ref().is_some_and(|(_, id)| *id == control_id) { slot.take(); true } else { false }
    });
    let menu = PENDING_MENU.with(|p| {
      let mut slot = p.borrow_mut();
      if slot.as_ref().is_some_and(|m| m.id() == control_id) { slot.take(); true } else { false }
    });
    if color || file || dialog || date || menu {
      notify_control(ControlEvent::Hide);
    }
  }

  fn notify_page_title_changed(&self, _webview: ServoWebView, title: Option<String>) {
    if let Some(handler) = &self.document_title_changed_handler {
      handler(title.unwrap_or_default());
    }
  }

  fn notify_load_status_changed(&self, webview: ServoWebView, status: LoadStatus) {
    if matches!(status, LoadStatus::Complete) {
      if let Some(url) = self.nav_gate.borrow_mut().initial_load_complete() {
        webview.load(url);
      }
    }
    let Some(handler) = &self.on_page_load_handler else {
      return;
    };
    let event = match status {
      LoadStatus::Started => PageLoadEvent::Started,
      LoadStatus::Complete => PageLoadEvent::Finished,
      LoadStatus::HeadParsed => return,
    };
    let url = webview.url().map(|url| url.to_string()).unwrap_or_default();
    handler(event, url);
  }

  fn request_navigation(&self, _webview: ServoWebView, request: NavigationRequest) {
    if self
      .navigation_handler
      .as_ref()
      .is_some_and(|handler| !handler(request.url.to_string()))
    {
      request.deny();
    } else {
      request.allow();
    }
  }

  /// Asks the app's resource filter about every load (tracker/ad blocking). Not intercepting drops
  /// `load`, which tells Servo to proceed with the real request.
  fn load_web_resource(&self, _webview: ServoWebView, load: WebResourceLoad) {
    use super::resource_filter::{filter_action, intercept_for, Interception, ResourceRequestInfo};
    let request = load.request();
    let action = filter_action(&ResourceRequestInfo {
      webview_label: &self.label,
      url: &request.url,
      method: &request.method,
      is_main_frame: request.is_for_main_frame,
    });
    if let Some(interception) = intercept_for(action) {
      match interception {
        Interception::Cancel => {
          // A network error, as content blockers produce. `load.intercept(..).finish()` with an
          // empty body panics libservo's subresource-integrity check on `integrity=` scripts.
          let url = load.request().url.clone();
          load.intercept(WebResourceResponse::new(url)).cancel();
        }
      }
    }
  }

  fn notify_new_frame_ready(&self, _webview: ServoWebView) {
    self.request_repaint();
  }

  fn notify_closed(&self, _webview: ServoWebView) {
    BY_LABEL.with(|m| m.borrow_mut().remove(&self.label));
    self.closed.set(true);
    self.waker.wake();
  }

  fn show_console_message(
    &self,
    webview: ServoWebView,
    _level: servo::ConsoleLogLevel,
    message: String,
  ) {
    let (Some(handler), Some(body)) = (self.ipc_handler.as_ref(), ipc_message_body(&message))
    else {
      return;
    };
    let uri = ipc_request_uri(webview.url().map(|url| url.to_string()).as_deref());
    let request = http::Request::builder()
      .uri(uri)
      .body(body.to_owned())
      .expect("the fallback IPC request URI is valid");
    handler(request);
  }
}

#[derive(Clone, Copy)]
struct PhysicalBounds {
  position: PhysicalPosition<i32>,
  size: PhysicalSize<u32>,
}

impl PhysicalBounds {
  fn from_rect(bounds: Rect, scale_factor: f64) -> Self {
    let position = bounds.position.to_physical::<i32>(scale_factor);
    let mut size = bounds.size.to_physical::<u32>(scale_factor);
    size.width = size.width.max(1);
    size.height = size.height.max(1);
    Self { position, size }
  }
}

enum RenderingTarget {
  Window {
    window: Rc<Window>,
    context: Rc<WindowRenderingContext>,
  },
  Child {
    window_id: tao::window::WindowId,
    child_id: ChildId,
    parent_context: Rc<WindowRenderingContext>,
    context: Rc<OffscreenRenderingContext>,
    bounds: Cell<Rect>,
    scale_factor: Cell<f64>,
  },
}

impl RenderingTarget {
  fn is_window(&self) -> bool {
    matches!(self, Self::Window { .. })
  }

  fn scale_factor(&self) -> f64 {
    match self {
      Self::Window { window, .. } => window.scale_factor(),
      Self::Child { scale_factor, .. } => scale_factor.get(),
    }
  }

  fn rendering_context(&self) -> Rc<dyn RenderingContext> {
    match self {
      Self::Window { context, .. } => context.clone(),
      Self::Child { context, .. } => context.clone(),
    }
  }

  fn paint(&self, webview: &ServoWebView) {
    webview.paint();

    match self {
      Self::Window { window, context } => {
        composite_children(window.id(), context);
        context.present();
      }
      // This child rendered into its offscreen buffer: re-blit every visible child of the
      // window in draw order and present once.
      Self::Child {
        window_id,
        parent_context,
        ..
      } => {
        composite_children(*window_id, parent_context);
        parent_context.present();
      }
    }
  }

  fn handle_parent_resized(&self, size: PhysicalSize<u32>, webview: &ServoWebView) {
    match self {
      Self::Window { .. } => webview.resize(non_zero_size(size)),
      Self::Child { parent_context, .. } => parent_context.resize(non_zero_size(size)),
    }
  }

  fn handle_scale_factor_changed(
    &self,
    scale_factor: f64,
    parent_size: PhysicalSize<u32>,
    webview: &ServoWebView,
  ) {
    webview.set_hidpi_scale_factor(Scale::new(scale_factor as f32));
    match self {
      Self::Window { .. } => webview.resize(non_zero_size(parent_size)),
      Self::Child {
        window_id,
        child_id,
        parent_context,
        bounds,
        scale_factor: current_scale_factor,
        ..
      } => {
        current_scale_factor.set(scale_factor);
        parent_context.resize(non_zero_size(parent_size));
        let bounds = PhysicalBounds::from_rect(bounds.get(), scale_factor);
        with_compositor(*window_id, |compositor| {
          compositor.set_bounds(*child_id, to_bounds(bounds))
        });
        // libservo's `resize_rendering_context` returns early when the context already has the
        // new size, so the offscreen context must NOT be resized first: `webview.resize` resizes
        // it and updates the viewport, display list and repaint.
        webview.resize(bounds.size);
      }
    }
  }

  fn bounds(&self) -> Rect {
    match self {
      Self::Window { window, .. } => {
        let size = window.inner_size();
        Rect {
          position: PhysicalPosition::new(0, 0).into(),
          size: size.into(),
        }
      }
      Self::Child { bounds, .. } => bounds.get(),
    }
  }

  fn set_bounds(&self, bounds: Rect, webview: &ServoWebView) {
    match self {
      Self::Window { window, .. } => {
        let size = bounds.size.to_physical::<u32>(window.scale_factor());
        webview.resize(non_zero_size(size));
      }
      Self::Child {
        window_id,
        child_id,
        bounds: current_bounds,
        scale_factor,
        ..
      } => {
        current_bounds.set(bounds);
        let bounds = PhysicalBounds::from_rect(bounds, scale_factor.get());
        with_compositor(*window_id, |compositor| {
          compositor.set_bounds(*child_id, to_bounds(bounds))
        });
        // libservo's `resize_rendering_context` returns early when the context already has the
        // new size, so the offscreen context must NOT be resized first: `webview.resize` resizes
        // it and updates the viewport, display list and repaint.
        webview.resize(bounds.size);
      }
    }
  }

  fn set_child_visible(&self, visible: bool) {
    if let Self::Child {
      window_id,
      child_id,
      ..
    } = self
    {
      with_compositor(*window_id, |compositor| {
        compositor.set_visible(*child_id, visible)
      });
    }
  }

  fn set_window_visible(&self, visible: bool) {
    if let Self::Window { window, .. } = self {
      window.set_visible(visible);
    }
  }

  fn focus_parent(&self) -> Result<()> {
    match self {
      Self::Window { window, .. } => {
        window.set_focus();
        Ok(())
      }
      Self::Child { .. } => Err(Error::Servo(
        "focusing the parent of an embedded Servo webview must be handled by the host runtime"
          .into(),
      )),
    }
  }

  fn webview_point(&self, position: PhysicalPosition<f64>) -> Option<DevicePoint> {
    match self {
      Self::Window { window, .. } => {
        let size = window.inner_size();
        point_in_bounds(position, PhysicalPosition::new(0, 0), size)
      }
      Self::Child {
        window_id,
        child_id,
        ..
      } => {
        let bounds = child_physical_bounds(*window_id, *child_id)?;
        // Input follows draw order: only the topmost visible child under the pointer gets it.
        let top = with_compositor(*window_id, |compositor| {
          compositor.hit_test(position.x.floor() as i32, position.y.floor() as i32)
        })
        .flatten();
        if top != Some(*child_id) {
          return None;
        }
        point_in_bounds(position, bounds.position, bounds.size)
      }
    }
  }
}

fn point_in_bounds(
  point: PhysicalPosition<f64>,
  origin: PhysicalPosition<i32>,
  size: PhysicalSize<u32>,
) -> Option<DevicePoint> {
  let x = point.x - f64::from(origin.x);
  let y = point.y - f64::from(origin.y);
  (x >= 0.0 && y >= 0.0 && x < f64::from(size.width) && y < f64::from(size.height))
    .then(|| DevicePoint::new(x as f32, y as f32))
}

fn servo_key(key: &TaoKey<'_>) -> Key {
  match key {
    TaoKey::Character(character) => Key::Character((*character).to_owned()),
    TaoKey::Space => Key::Character(" ".into()),
    TaoKey::Super => Key::Named(NamedKey::Meta),
    TaoKey::Unidentified(_) | TaoKey::Dead(_) => Key::Named(NamedKey::Unidentified),
    key => Key::Named(format!("{key:?}").parse().unwrap_or(NamedKey::Unidentified)),
  }
}

fn servo_code(code: TaoKeyCode) -> Code {
  match code {
    TaoKeyCode::SuperLeft => Code::MetaLeft,
    TaoKeyCode::SuperRight => Code::MetaRight,
    TaoKeyCode::Unidentified(_) => Code::Unidentified,
    code => code.to_string().parse().unwrap_or(Code::Unidentified),
  }
}

fn servo_location(location: KeyLocation) -> Location {
  match location {
    KeyLocation::Standard => Location::Standard,
    KeyLocation::Left => Location::Left,
    KeyLocation::Right => Location::Right,
    KeyLocation::Numpad => Location::Numpad,
    _ => Location::Standard,
  }
}

fn servo_modifiers(modifiers: ModifiersState) -> Modifiers {
  let mut result = Modifiers::empty();
  result.set(Modifiers::SHIFT, modifiers.shift_key());
  result.set(Modifiers::CONTROL, modifiers.control_key());
  result.set(Modifiers::ALT, modifiers.alt_key());
  result.set(Modifiers::META, modifiers.super_key());
  result
}

fn servo_mouse_button(button: &TaoMouseButton) -> Option<servo::MouseButton> {
  match button {
    TaoMouseButton::Left => Some(servo::MouseButton::Left),
    TaoMouseButton::Right => Some(servo::MouseButton::Right),
    TaoMouseButton::Middle => Some(servo::MouseButton::Middle),
    TaoMouseButton::Other(button) => Some(servo::MouseButton::Other(*button)),
    _ => None,
  }
}

fn servo_wheel_delta(delta: &MouseScrollDelta) -> Option<WheelDelta> {
  let (x, y) = match delta {
    MouseScrollDelta::LineDelta(x, y) => ((x * 76.0) as f64, (y * 76.0) as f64),
    MouseScrollDelta::PixelDelta(delta) => (delta.x, delta.y),
    _ => return None,
  };
  Some(WheelDelta {
    x,
    y,
    z: 0.0,
    mode: WheelMode::DeltaPixel,
  })
}

fn servo_touch_phase(phase: TouchPhase) -> Option<TouchEventType> {
  match phase {
    TouchPhase::Started => Some(TouchEventType::Down),
    TouchPhase::Moved => Some(TouchEventType::Move),
    TouchPhase::Ended => Some(TouchEventType::Up),
    TouchPhase::Cancelled => Some(TouchEventType::Cancel),
    _ => None,
  }
}

fn inserted_key_text(
  key: &TaoKey<'_>,
  text: Option<&str>,
  mut modifiers: Modifiers,
) -> Option<String> {
  modifiers.remove(Modifiers::SHIFT);
  if !modifiers.is_empty() {
    return None;
  }
  match key {
    // Prefer the logical key over `KeyEvent::text`: tao's X11 backend fills
    // `text` with the unshifted character ("t" while the logical key is
    // "T"), so a dedup keyed on `text` misses shifted characters — the
    // matching `ReceivedImeText` then commits a second copy of every
    // shift-modified character into editable content.
    TaoKey::Character(character) => Some((*character).to_owned()),
    TaoKey::Space => Some(" ".into()),
    _ => text.map(ToOwned::to_owned),
  }
}

fn non_zero_size(mut size: PhysicalSize<u32>) -> PhysicalSize<u32> {
  size.width = size.width.max(1);
  size.height = size.height.max(1);
  size
}

thread_local! {
  /// One `Servo` engine per process: libservo's global options can only be initialized once, so
  /// every webview (toolbar and page children) shares this engine.
  static SHARED_SERVO: RefCell<Option<Rc<Servo>>> = const { RefCell::new(None) };
}

/// Everything that composes one native window: its single window context, the children's
/// offscreen contexts, and the `Compositor` that owns their order, visibility and bounds.
struct WindowComp {
  #[allow(dead_code)]
  window: Option<Rc<Window>>,
  context: Rc<WindowRenderingContext>,
  compositor: Compositor,
  children: HashMap<ChildId, Rc<OffscreenRenderingContext>>,
  /// Tauri webview label -> child id, so the app can reorder children by label.
  labels: HashMap<String, ChildId>,
}

thread_local! {
  static WINDOWS: RefCell<HashMap<tao::window::WindowId, WindowComp>> = RefCell::new(HashMap::new());
  static NEXT_CHILD_ID: Cell<ChildId> = const { Cell::new(1) };
  /// Live webviews using the shared engine (see `EngineRegistry`).
  static LIVE: RefCell<EngineRegistry> = RefCell::new(EngineRegistry::new());
}

/// Number of webviews currently alive (toolbar and page children).
pub fn live_webview_count() -> usize {
  LIVE.with(|live| live.borrow().live())
}

/// Children registered with the per-window compositors, summed over all windows.
pub fn compositor_child_count() -> usize {
  WINDOWS.with(|windows| windows.borrow().values().map(|comp| comp.compositor.len()).sum())
}

/// Moves the child with this Tauri webview label to the front of its window's draw order (and of
/// input hit-testing) and redraws. Must be called on the main thread (use
/// `AppHandle::run_on_main_thread`). Returns whether a child with that label exists.
pub fn bring_to_front(label: &str) -> bool {
  let found = WINDOWS.with(|windows| {
    let mut windows = windows.borrow_mut();
    for (window_id, comp) in windows.iter_mut() {
      if let Some(child_id) = comp.labels.get(label).copied() {
        comp.compositor.bring_to_front(child_id);
        return Some((*window_id, comp.context.clone()));
      }
    }
    None
  });
  let Some((window_id, context)) = found else {
    return false;
  };
  composite_children(window_id, &context);
  context.present();
  true
}

/// Draws the rectangle `(x, y, width, height)` (window pixels) of the webview with this label above
/// every other webview of its window, and sends pointer input there; `None` clears it. For a
/// dropdown drawn by the toolbar webview that must overlay the page webview in front of it. Must be
/// called on the main thread. Returns whether a webview with that label exists.
pub fn set_overlay_region(label: &str, rect: Option<(i32, i32, u32, u32)>) -> bool {
  let found = WINDOWS.with(|windows| {
    let mut windows = windows.borrow_mut();
    for (window_id, comp) in windows.iter_mut() {
      if let Some(child_id) = comp.labels.get(label).copied() {
        comp.compositor.set_overlay(rect.map(|(x, y, width, height)| Overlay {
          id: child_id,
          bounds: Bounds { x, y, width, height },
        }));
        return Some((*window_id, comp.context.clone()));
      }
    }
    None
  });
  let Some((window_id, context)) = found else {
    return false;
  };
  composite_children(window_id, &context);
  context.present();
  true
}

/// One row of a `<select>` popup: an option, or a non-selectable group heading.
#[derive(Clone, Debug, PartialEq)]
pub struct SelectItem {
  /// Pass this back in `submit_select`/`update_select` to choose the option.
  pub id: usize,
  pub label: String,
  pub disabled: bool,
  /// True for an `<optgroup>` heading.
  pub group: bool,
  /// True for an option inside an `<optgroup>`.
  pub indented: bool,
}

/// A `<select>` the page opened, for the app to draw.
#[derive(Clone, Debug)]
pub struct SelectPopup {
  /// Tauri label of the webview that owns the `<select>`.
  pub label: String,
  /// The `<select>` element's box `(x, y, width, height)` in that webview's pixels.
  pub rect: (i32, i32, u32, u32),
  pub items: Vec<SelectItem>,
  /// Ids of the options selected now.
  pub selected: Vec<usize>,
  pub multiple: bool,
}

#[derive(Clone, Debug)]
pub enum SelectEvent {
  Show(SelectPopup),
  /// The page closed the picker itself (navigation, element removed, ...).
  Hide,
}

/// Flattens options and `<optgroup>`s into popup rows (pure, tested).
pub(crate) fn flatten_select_options(options: &[servo::SelectElementOptionOrOptgroup]) -> Vec<SelectItem> {
  let mut out = Vec::new();
  for entry in options {
    match entry {
      servo::SelectElementOptionOrOptgroup::Option(o) => out.push(SelectItem {
        id: o.id,
        label: o.label.clone(),
        disabled: o.is_disabled,
        group: false,
        indented: false,
      }),
      servo::SelectElementOptionOrOptgroup::Optgroup { label, options } => {
        out.push(SelectItem { id: usize::MAX, label: label.clone(), disabled: true, group: true, indented: false });
        for o in options {
          out.push(SelectItem { id: o.id, label: o.label.clone(), disabled: o.is_disabled, group: false, indented: true });
        }
      }
    }
  }
  out
}

type SelectHandler = Arc<dyn Fn(SelectEvent) + Send + Sync>;

static SELECT_HANDLER: std::sync::Mutex<Option<SelectHandler>> = std::sync::Mutex::new(None);

thread_local! {
  /// The `<select>` picker waiting for the app's answer (main thread only).
  static PENDING_SELECT: RefCell<Option<servo::SelectElement>> = const { RefCell::new(None) };
}

fn notify_select(event: SelectEvent) {
  let handler = SELECT_HANDLER.lock().ok().and_then(|h| h.clone());
  if let Some(handler) = handler {
    handler(event);
  }
}

/// Registers the callback that draws `<select>` popups. It runs on the main thread inside the
/// engine's callbacks, so it must not call back into the webviews synchronously. Without a
/// handler, a `<select>` does nothing when clicked.
pub fn set_select_handler(handler: impl Fn(SelectEvent) + Send + Sync + 'static) {
  if let Ok(mut slot) = SELECT_HANDLER.lock() {
    *slot = Some(Arc::new(handler));
  }
}

/// Changes the pending `<select>`'s selection without closing it (for multi-selects). Main thread.
pub fn update_select(selected: Vec<usize>) {
  PENDING_SELECT.with(|p| {
    if let Some(select) = p.borrow_mut().as_mut() {
      select.select(selected);
    }
  });
}

/// Applies `selected` to the pending `<select>` and closes it. Main thread.
pub fn submit_select(selected: Vec<usize>) {
  if let Some(mut select) = PENDING_SELECT.with(|p| p.borrow_mut().take()) {
    select.select(selected);
    select.submit();
  }
  with_shared_servo(|servo| servo.spin_event_loop());
}

/// Closes the pending `<select>` keeping whatever was last selected. Main thread.
pub fn cancel_select() {
  PENDING_SELECT.with(|p| p.borrow_mut().take());
  with_shared_servo(|servo| servo.spin_event_loop());
}

/// A date or time input (`date`, `datetime-local`, `month`, `time`, `week`) for the app to draw.
#[derive(Clone, Debug)]
pub struct DatePopup {
  pub label: String,
  /// The input's box `(x, y, width, height)` in that webview's page pixels.
  pub rect: (i32, i32, u32, u32),
  pub kind: &'static str,
  /// The input's current value in HTML's own format ("2026-11-20", "14:30", ...); may be empty.
  pub value: String,
}

/// One row of a right-click menu.
#[derive(Clone, Debug)]
pub enum MenuEntry {
  Item { label: String, enabled: bool },
  Separator,
}

/// A right-click menu for the app to draw at the pointer.
#[derive(Clone, Debug)]
pub struct MenuPopup {
  pub label: String,
  /// Where the pointer was, in device pixels of that webview.
  pub x: f32,
  pub y: f32,
  pub items: Vec<MenuEntry>,
}

/// A colour picker (`<input type=color>`) for the app to draw.
#[derive(Clone, Debug)]
pub struct ColorPopup {
  pub label: String,
  /// The input's box `(x, y, width, height)` in that webview's page pixels.
  pub rect: (i32, i32, u32, u32),
  pub current: Option<(u8, u8, u8)>,
}

/// A file chooser (`<input type=file>`): the app shows its native dialog.
#[derive(Clone, Debug)]
pub struct FilePopup {
  pub label: String,
  /// Allowed extensions without the dot, or empty for any file.
  pub patterns: Vec<String>,
  pub multiple: bool,
  pub current: Vec<std::path::PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DialogKind {
  Alert,
  Confirm,
  Prompt,
}

/// A script dialog: `alert()`, `confirm()` or `prompt()`. The page waits until it is answered.
#[derive(Clone, Debug)]
pub struct DialogPopup {
  pub label: String,
  pub kind: DialogKind,
  pub message: String,
  /// The prompt's default text (empty for the others).
  pub default: String,
}

#[derive(Clone, Debug)]
pub enum ControlEvent {
  Date(DatePopup),
  Menu(MenuPopup),
  Color(ColorPopup),
  File(FilePopup),
  Dialog(DialogPopup),
  /// A navigation key pressed while a popup is open: "ArrowUp", "ArrowDown", "Home", "End",
  /// "Enter" or "Escape". The engine does not pass these on to the page.
  Key(&'static str),
  /// The page resolved a colour picker, file chooser or dialog by itself.
  Hide,
}

type ControlHandler = Arc<dyn Fn(ControlEvent) + Send + Sync>;

static CONTROL_HANDLER: std::sync::Mutex<Option<ControlHandler>> = std::sync::Mutex::new(None);

thread_local! {
  static PENDING_COLOR: RefCell<Option<(servo::ColorPicker, String)>> = const { RefCell::new(None) };
  static PENDING_DATE: RefCell<Option<(String, servo::EmbedderControlId)>> = const { RefCell::new(None) };
  static PENDING_MENU: RefCell<Option<servo::ContextMenu>> = const { RefCell::new(None) };
  /// The pointer's last position over any webview (device pixels), where a right-click menu opens.
  static LAST_CURSOR: std::cell::Cell<(f32, f32)> = const { std::cell::Cell::new((0.0, 0.0)) };
  static PENDING_FILE: RefCell<Option<servo::FilePicker>> = const { RefCell::new(None) };
  static PENDING_DIALOG: RefCell<Option<(servo::SimpleDialog, servo::EmbedderControlId)>> = const { RefCell::new(None) };
}

fn notify_control(event: ControlEvent) {
  let handler = CONTROL_HANDLER.lock().ok().and_then(|h| h.clone());
  if let Some(handler) = handler {
    handler(event);
  }
}

/// Registers the callback for colour pickers, file choosers, script dialogs and popup keys. Runs
/// on the main thread inside the engine's callbacks (do not call into the webviews from it).
/// Without it those controls keep the engine's defaults.
pub fn set_control_handler(handler: impl Fn(ControlEvent) + Send + Sync + 'static) {
  if let Ok(mut slot) = CONTROL_HANDLER.lock() {
    *slot = Some(Arc::new(handler));
  }
}

/// Answers the pending colour picker with `color` (None keeps the page's value). Main thread.
pub fn submit_color(color: Option<(u8, u8, u8)>) {
  if let Some((mut picker, label)) = PENDING_COLOR.with(|p| p.borrow_mut().take()) {
    let chose = color.is_some();
    if let Some((red, green, blue)) = color {
      picker.select(Some(servo::RgbColor { red, green, blue }));
    }
    picker.submit();
    // The engine sets the input's value but fires no events, and pages listen for them. Fire the
    // same `input` and `change` a native picker would, on the colour input that was clicked.
    if chose {
      if let Some(webview) = BY_LABEL.with(|m| m.borrow().get(&label).cloned()) {
        webview.evaluate_javascript(COLOR_EVENTS_JS, |_| {});
      }
    }
  }
  with_shared_servo(|servo| servo.spin_event_loop());
}

const COLOR_EVENTS_JS: &str = "(function(){var e=document.activeElement;\
if(e&&e.tagName==='INPUT'&&e.type==='color'){\
e.dispatchEvent(new Event('input',{bubbles:true}));\
e.dispatchEvent(new Event('change',{bubbles:true}));}})()";

/// Answers the pending date or time picker with the new value in HTML's format, or None to leave
/// the input alone. An empty string clears it. Main thread.
pub fn submit_date(value: Option<String>) {
  if let Some((label, _)) = PENDING_DATE.with(|p| p.borrow_mut().take()) {
    if let Some(value) = value {
      if let Some(webview) = BY_LABEL.with(|m| m.borrow().get(&label).cloned()) {
        // Only the characters of an HTML date or time value are let through, so nothing in it can
        // break out of the quotes.
        let clean: String = value.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | ':' | 'T' | '.')).collect();
        let quoted = format!("'{clean}'");
        let js = format!(
          "(function(){{var e=window.__bifrostDateTarget||document.activeElement;\
if(e&&e.tagName==='INPUT'){{e.value={quoted};\
e.dispatchEvent(new Event('input',{{bubbles:true}}));\
e.dispatchEvent(new Event('change',{{bubbles:true}}));}}}})()"
        );
        webview.evaluate_javascript(js, |_| {});
      }
    }
  }
  with_shared_servo(|servo| servo.spin_event_loop());
}

/// Answers the pending right-click menu with the row the user chose (an index into the rows sent),
/// or None to dismiss it. Returns a URL the app should open in a new tab, for "Open Link/Image in
/// New Tab": the app owns its tabs, so those two are not left to the engine. Main thread.
pub fn answer_menu(index: Option<usize>) -> Option<String> {
  let menu = PENDING_MENU.with(|p| p.borrow_mut().take())?;
  let mut open = None;
  match index.and_then(|i| menu.items().get(i).cloned()) {
    Some(servo::ContextMenuItem::Item { action, enabled: true, .. }) => match action {
      servo::ContextMenuAction::OpenLinkInNewWebView => {
        open = menu.element_info().link_url.as_ref().map(|u| u.to_string());
        menu.dismiss();
      }
      servo::ContextMenuAction::OpenImageInNewView => {
        open = menu.element_info().image_url.as_ref().map(|u| u.to_string());
        menu.dismiss();
      }
      action => menu.select(action),
    },
    _ => menu.dismiss(),
  }
  with_shared_servo(|servo| servo.spin_event_loop());
  open
}

/// Sends a trusted right-click at `(x, y)` (that webview's own pixels). For automated tests.
/// Main thread. Returns whether the webview exists.
pub fn inject_right_click(label: &str, x: f32, y: f32) -> bool {
  let Some(webview) = BY_LABEL.with(|m| m.borrow().get(label).cloned()) else {
    return false;
  };
  LAST_CURSOR.with(|c| c.set((x, y)));
  let point = DevicePoint::new(x, y);
  webview.notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(point.into())));
  for action in [MouseButtonAction::Down, MouseButtonAction::Up] {
    webview.notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
      action,
      servo::MouseButton::Right,
      point.into(),
    )));
  }
  with_shared_servo(|servo| servo.spin_event_loop());
  true
}

/// Answers the pending file chooser: `Some(paths)` attaches them, `None` cancels. Main thread.
pub fn submit_files(paths: Option<Vec<std::path::PathBuf>>) {
  if let Some(mut picker) = PENDING_FILE.with(|p| p.borrow_mut().take()) {
    match paths {
      Some(paths) => {
        picker.select(&paths);
        picker.submit();
      }
      None => picker.dismiss(),
    }
  }
  with_shared_servo(|servo| servo.spin_event_loop());
}

/// Answers the pending script dialog. `accept` is OK (true) or Cancel (false); `text` is the
/// prompt's answer. Main thread.
pub fn answer_dialog(accept: bool, text: Option<String>) {
  if let Some((dialog, _)) = PENDING_DIALOG.with(|p| p.borrow_mut().take()) {
    match dialog {
      servo::SimpleDialog::Prompt(mut prompt) => {
        if accept {
          if let Some(text) = text {
            prompt.set_current_value(&text);
          }
          prompt.confirm();
        } else {
          prompt.dismiss();
        }
      }
      other => {
        if accept {
          other.confirm();
        } else {
          other.dismiss();
        }
      }
    }
  }
  with_shared_servo(|servo| servo.spin_event_loop());
}

/// True while any popup (select list, colour picker, file chooser or dialog) waits for an answer.
pub fn popup_pending() -> bool {
  PENDING_SELECT.with(|p| p.borrow().is_some())
    || PENDING_COLOR.with(|p| p.borrow().is_some())
    || PENDING_FILE.with(|p| p.borrow().is_some())
    || PENDING_DIALOG.with(|p| p.borrow().is_some())
    || PENDING_DATE.with(|p| p.borrow().is_some())
    || PENDING_MENU.with(|p| p.borrow().is_some())
}

/// Name of a key that drives popups, or None for every other key.
pub(crate) fn popup_key_name(key: &TaoKey<'_>) -> Option<&'static str> {
  match key {
    TaoKey::ArrowUp => Some("ArrowUp"),
    TaoKey::ArrowDown => Some("ArrowDown"),
    TaoKey::Home => Some("Home"),
    TaoKey::End => Some("End"),
    TaoKey::Enter => Some("Enter"),
    TaoKey::Escape => Some("Escape"),
    _ => None,
  }
}

/// Whether the open popup (if any) takes this key. Enter and Escape go to any popup; the cursor
/// keys only to a select list, because a colour picker's or dialog's text box needs them to edit.
pub(crate) fn popup_wants_key(name: &str) -> bool {
  match name {
    "Enter" | "Escape" => popup_pending(),
    _ => PENDING_SELECT.with(|p| p.borrow().is_some()) || PENDING_MENU.with(|p| p.borrow().is_some()),
  }
}

/// Sends a popup key to the app. For automated tests (the window-event path calls the same code).
/// Main thread. Returns whether a popup was waiting for that key.
pub fn inject_popup_key(name: &'static str) -> bool {
  if !popup_wants_key(name) {
    return false;
  }
  notify_control(ControlEvent::Key(name));
  true
}

fn with_shared_servo(f: impl FnOnce(&Servo)) {
  SHARED_SERVO.with(|cell| {
    if let Some(servo) = cell.borrow().as_ref() {
      f(servo);
    }
  });
}

thread_local! {
  /// Live webviews by Tauri label, for `inject_click`.
  static BY_LABEL: RefCell<HashMap<String, ServoWebView>> = RefCell::new(HashMap::new());
}

/// Sends a trusted mouse click to the webview with this label at `(x, y)` (that webview's own
/// pixels), through the same engine input path as a real click. For automated tests: scripted
/// `element.click()` is not trusted and skips parts of the engine (such as `<select>` pickers).
/// Main thread. Returns whether the webview exists.
pub fn inject_click(label: &str, x: f32, y: f32) -> bool {
  let Some(webview) = BY_LABEL.with(|m| m.borrow().get(label).cloned()) else {
    return false;
  };
  let point = DevicePoint::new(x, y);
  webview.notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(point.into())));
  for action in [MouseButtonAction::Down, MouseButtonAction::Up] {
    webview.notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
      action,
      servo::MouseButton::Left,
      point.into(),
    )));
  }
  with_shared_servo(|servo| servo.spin_event_loop());
  true
}

type DismissHandler = Arc<dyn Fn() + Send + Sync>;

static OVERLAY_DISMISS_HANDLER: std::sync::Mutex<Option<DismissHandler>> = std::sync::Mutex::new(None);

/// Registers a callback run (on the main thread) when the user presses the mouse outside an active
/// overlay region, after the overlay has been cleared. The app uses it to close its dropdown: clicks
/// on the page in front never reach the toolbar page that owns the dropdown. The callback must not
/// call back into the webviews synchronously.
pub fn set_overlay_dismiss_handler(handler: impl Fn() + Send + Sync + 'static) {
  if let Ok(mut slot) = OVERLAY_DISMISS_HANDLER.lock() {
    *slot = Some(Arc::new(handler));
  }
}

/// Releases the shared engine at event-loop exit, once no webview remains. libservo cannot be
/// re-initialized in the same process, so the engine is never dropped earlier.
pub(crate) fn release_engine_if_idle() {
  if LIVE.with(|live| live.borrow().is_idle()) {
    SHARED_SERVO.with(|cell| cell.borrow_mut().take());
  }
}

fn next_child_id() -> ChildId {
  NEXT_CHILD_ID.with(|next| {
    let id = next.get();
    next.set(id + 1);
    id
  })
}

fn to_bounds(bounds: PhysicalBounds) -> Bounds {
  Bounds {
    x: bounds.position.x,
    y: bounds.position.y,
    width: bounds.size.width,
    height: bounds.size.height,
  }
}

fn from_bounds(bounds: Bounds) -> PhysicalBounds {
  PhysicalBounds {
    position: PhysicalPosition::new(bounds.x, bounds.y),
    size: PhysicalSize::new(bounds.width, bounds.height),
  }
}

fn with_compositor<T>(
  window_id: tao::window::WindowId,
  f: impl FnOnce(&mut Compositor) -> T,
) -> Option<T> {
  WINDOWS.with(|windows| {
    windows
      .borrow_mut()
      .get_mut(&window_id)
      .map(|comp| f(&mut comp.compositor))
  })
}

fn child_physical_bounds(
  window_id: tao::window::WindowId,
  child_id: ChildId,
) -> Option<PhysicalBounds> {
  with_compositor(window_id, |compositor| compositor.bounds(child_id))
    .flatten()
    .map(from_bounds)
}

/// Draws every visible child of a window into the shared window context, back to front. Each
/// child keeps its last rendered frame in its offscreen context, so any child (or the window)
/// can trigger a full redraw and one present without clobbering the others.
fn composite_children(window_id: tao::window::WindowId, parent_context: &WindowRenderingContext) {
  WINDOWS.with(|windows| {
    let windows = windows.borrow();
    let Some(comp) = windows.get(&window_id) else {
      return;
    };
    if parent_context.make_current().is_err() {
      return;
    }
    parent_context.prepare_for_rendering();
    let parent_height = parent_context.size().height as i32;
    for id in comp.compositor.draw_order() {
      let (Some(context), Some(bounds)) = (comp.children.get(&id), comp.compositor.bounds(id))
      else {
        continue;
      };
      if let Some(render_to_parent) = context.render_to_parent_callback() {
        let (width, height) = (bounds.width as i32, bounds.height as i32);
        let target = EuclidRect::new(
          Point2D::new(bounds.x, parent_height - bounds.y - height),
          Size2D::new(width, height),
        );
        render_to_parent(parent_context.glow_gl_api().as_ref(), target);
      }
    }
    if let Some(overlay) = comp.compositor.overlay() {
      draw_overlay(parent_context, comp, overlay);
    }
  });
}

/// Paints `overlay.bounds` of the overlay child's last frame on top of the window, over whatever the
/// other children drew there. The layers cannot be alpha-blended (libservo copies them opaquely), so
/// the region is read back from the child and copied across: used for a dropdown that belongs to the
/// toolbar layer but must show above the page layer.
fn draw_overlay(parent_context: &WindowRenderingContext, comp: &WindowComp, overlay: Overlay) {
  use glow::HasContext as _;
  let (Some(child), Some(child_bounds)) =
    (comp.children.get(&overlay.id), comp.compositor.bounds(overlay.id))
  else {
    return;
  };
  // Overlay rectangle in the child's own pixels, clamped to its frame.
  let x = (overlay.bounds.x - child_bounds.x).max(0);
  let y = (overlay.bounds.y - child_bounds.y).max(0);
  let w = (overlay.bounds.width as i32).min(child_bounds.width as i32 - x);
  let h = (overlay.bounds.height as i32).min(child_bounds.height as i32 - y);
  if w <= 0 || h <= 0 {
    return;
  }
  // read_to_image takes GL coordinates (origin bottom-left) and returns the rows top-first.
  let gl_y = child_bounds.height as i32 - y - h;
  let Some(image) = child.read_to_image(DeviceIntRect::from_origin_and_size(
    servo::DeviceIntPoint::new(x, gl_y),
    servo::DeviceIntSize::new(w, h),
  ))
  else {
    return;
  };
  let gl = parent_context.glow_gl_api();
  parent_context.prepare_for_rendering();
  let parent_height = parent_context.size().height as i32;
  let dest_x = overlay.bounds.x.max(0);
  let dest_top = parent_height - overlay.bounds.y.max(0);
  unsafe {
    let window_fbo = u32::try_from(gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING)).unwrap_or(0);
    let Ok(texture) = gl.create_texture() else {
      return;
    };
    gl.bind_texture(glow::TEXTURE_2D, Some(texture));
    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::NEAREST as i32);
    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::NEAREST as i32);
    gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
    gl.tex_image_2d(
      glow::TEXTURE_2D,
      0,
      glow::RGBA8 as i32,
      w,
      h,
      0,
      glow::RGBA,
      glow::UNSIGNED_BYTE,
      glow::PixelUnpackData::Slice(Some(image.as_raw())),
    );
    if let Ok(source) = gl.create_framebuffer() {
      gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(source));
      gl.framebuffer_texture_2d(
        glow::READ_FRAMEBUFFER,
        glow::COLOR_ATTACHMENT0,
        glow::TEXTURE_2D,
        Some(texture),
        0,
      );
      gl.bind_framebuffer(
        glow::DRAW_FRAMEBUFFER,
        std::num::NonZeroU32::new(window_fbo).map(glow::NativeFramebuffer),
      );
      // Rows are top-first but a GL framebuffer's row 0 is the bottom: flip with an inverted
      // destination (top edge first).
      gl.blit_framebuffer(0, 0, w, h, dest_x, dest_top, dest_x + w, dest_top - h, glow::COLOR_BUFFER_BIT, glow::NEAREST);
      gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
      gl.delete_framebuffer(source);
    }
    gl.bind_texture(glow::TEXTURE_2D, None);
    gl.delete_texture(texture);
  }
}

/// Owns a handle to the shared Servo engine, a rendering context, and one webview.
pub struct Embedder {
  servo: Rc<Servo>,
  webview: ServoWebView,
  target: RenderingTarget,
  delegate: Rc<Delegate>,
  cursor_position: Cell<Option<DevicePoint>>,
  modifiers: Cell<Modifiers>,
  pending_ime_text: RefCell<Option<String>>,
  pending_key_text: RefCell<Option<String>>,
  pending_key_event: Cell<bool>,
  focused: Cell<bool>,
}

impl Drop for Embedder {
  fn drop(&mut self) {
    let _ = LIVE.try_with(|live| live.borrow_mut().release());
    if let Ok(mut table) = ROUTES.lock() {
      table.unregister(&self.webview.id());
    }
    if let RenderingTarget::Child {
      window_id,
      child_id,
      ..
    } = &self.target
    {
      let _ = WINDOWS.try_with(|windows| {
        if let Some(comp) = windows.borrow_mut().get_mut(window_id) {
          comp.compositor.remove(*child_id);
          comp.children.remove(child_id);
          comp.labels.remove(&self.delegate.label);
        }
      });
    }
  }
}

impl Embedder {
  #[allow(clippy::too_many_arguments)]
  pub fn new(
    window: Window,
    proxy: EventLoopProxy<()>,
    webview_id: String,
    initial_url: Url,
    initial_headers: Option<http::HeaderMap>,
    background_color: Option<[f64; 4]>,
    initialization_scripts: Vec<InitializationScript>,
    ipc_handler: Option<Box<dyn Fn(http::Request<String>)>>,
    custom_protocols: HashMap<String, CustomProtocolHandler>,
    navigation_handler: Option<Box<dyn Fn(String) -> bool>>,
    document_title_changed_handler: Option<Box<dyn Fn(String)>>,
    on_page_load_handler: Option<Box<dyn Fn(PageLoadEvent, String)>>,
  ) -> Result<Self> {
    let window = Rc::new(window);
    let context = Rc::new(
      WindowRenderingContext::new(
        window.display_handle()?,
        window.window_handle()?,
        non_zero_size(window.inner_size()),
      )
      .map_err(|error| Error::Servo(format!("failed to create rendering context: {error:?}")))?,
    );
    WINDOWS.with(|windows| {
      windows.borrow_mut().insert(
        window.id(),
        WindowComp {
          window: Some(window.clone()),
          context: context.clone(),
          compositor: Compositor::new(),
          children: HashMap::new(),
          labels: HashMap::new(),
        },
      );
    });
    let wake_proxy = proxy.clone();
    let waker = EmbedderWaker::new(move || {
      if let Err(error) = wake_proxy.send_event(()) {
        eprintln!("Servo failed to wake the Tao event loop: {error}");
      }
    });
    let delegate = Rc::new(Delegate {
      window: Some(window.clone()),
      waker: waker.clone(),
      label: webview_id.clone(),
      nav_gate: RefCell::new(NavGate::new(
        std::time::Instant::now(),
        std::time::Duration::from_secs(2),
      )),
      frame_ready: Cell::new(false),
      closed: Cell::new(false),
      ipc_handler,
      navigation_handler,
      document_title_changed_handler,
      on_page_load_handler,
    });
    let target = RenderingTarget::Window { window, context };
    Self::build(
      target,
      waker,
      delegate,
      webview_id,
      initial_url,
      initial_headers,
      background_color,
      initialization_scripts,
      custom_protocols,
    )
  }

  #[allow(clippy::too_many_arguments)]
  pub fn new_child(
    parent: &Window,
    wake: impl Fn() + Send + Sync + 'static,
    webview_id: String,
    bounds: Rect,
    initial_url: Url,
    initial_headers: Option<http::HeaderMap>,
    background_color: Option<[f64; 4]>,
    initialization_scripts: Vec<InitializationScript>,
    ipc_handler: Option<Box<dyn Fn(http::Request<String>)>>,
    custom_protocols: HashMap<String, CustomProtocolHandler>,
    navigation_handler: Option<Box<dyn Fn(String) -> bool>>,
    document_title_changed_handler: Option<Box<dyn Fn(String)>>,
    on_page_load_handler: Option<Box<dyn Fn(PageLoadEvent, String)>>,
  ) -> Result<Self> {
    let window_id = parent.id();
    // All children of one native window share a single window rendering context.
    let parent_context = WINDOWS.with(|windows| -> Result<Rc<WindowRenderingContext>> {
      let mut windows = windows.borrow_mut();
      if let Some(comp) = windows.get(&window_id) {
        return Ok(comp.context.clone());
      }
      let context = Rc::new(
        WindowRenderingContext::new(
          parent.display_handle()?,
          parent.window_handle()?,
          non_zero_size(parent.inner_size()),
        )
        .map_err(|error| {
          Error::Servo(format!(
            "failed to create parent rendering context: {error:?}"
          ))
        })?,
      );
      windows.insert(
        window_id,
        WindowComp {
          window: None,
          context: context.clone(),
          compositor: Compositor::new(),
          children: HashMap::new(),
          labels: HashMap::new(),
        },
      );
      Ok(context)
    })?;
    let scale_factor = parent.scale_factor();
    let physical_bounds = PhysicalBounds::from_rect(bounds, scale_factor);
    let context = Rc::new(parent_context.offscreen_context(physical_bounds.size));
    let child_id = next_child_id();
    WINDOWS.with(|windows| {
      if let Some(comp) = windows.borrow_mut().get_mut(&window_id) {
        comp.compositor.add(child_id, to_bounds(physical_bounds));
        comp.children.insert(child_id, context.clone());
        comp.labels.insert(webview_id.clone(), child_id);
      }
    });
    let waker = EmbedderWaker::new(wake);
    let delegate = Rc::new(Delegate {
      window: None,
      waker: waker.clone(),
      label: webview_id.clone(),
      nav_gate: RefCell::new(NavGate::new(
        std::time::Instant::now(),
        std::time::Duration::from_secs(2),
      )),
      frame_ready: Cell::new(false),
      closed: Cell::new(false),
      ipc_handler,
      navigation_handler,
      document_title_changed_handler,
      on_page_load_handler,
    });
    let target = RenderingTarget::Child {
      window_id,
      child_id,
      parent_context,
      context,
      bounds: Cell::new(bounds),
      scale_factor: Cell::new(scale_factor),
    };
    Self::build(
      target,
      waker,
      delegate,
      webview_id,
      initial_url,
      initial_headers,
      background_color,
      initialization_scripts,
      custom_protocols,
    )
  }

  #[allow(clippy::too_many_arguments)]
  fn build(
    target: RenderingTarget,
    waker: EmbedderWaker,
    delegate: Rc<Delegate>,
    webview_id: String,
    initial_url: Url,
    initial_headers: Option<http::HeaderMap>,
    background_color: Option<[f64; 4]>,
    initialization_scripts: Vec<InitializationScript>,
    custom_protocols: HashMap<String, CustomProtocolHandler>,
  ) -> Result<Self> {
    target
      .rendering_context()
      .make_current()
      .map_err(|error| Error::Servo(format!("failed to activate rendering context: {error:?}")))?;

    let mut preferences = Preferences::default();
    if let Some(background_color) = background_color {
      preferences.shell_background_color_rgba = background_color;
    }
    // The legacy editing API (`document.queryCommandSupported` and friends)
    // is implemented but pref-gated off by default; widely-deployed libraries
    // probe it at load time (e.g. monaco-editor's clipboard contrib) and an
    // entire bundle dies on the missing function when the pref is off.
    preferences.dom_exec_command_enabled = true;
    // CSS Grid is implemented but ships disabled (layout_grid_enabled defaults
    // to false in servo's components/config/prefs.rs). With it off the
    // display:grid declaration does not parse and is dropped, so every grid
    // container silently falls back to its default display -- a <span> stays
    // inline. Nothing errors; the layout is just quietly wrong, which is how
    // this survived unnoticed: the app's disclosure caret painted as a
    // trapezoid only because its container was never a grid, leaving the
    // ::before in a line box.
    preferences.layout_grid_enabled = true;
    // Native SVG layout (servo-patches 0009-0024). The pref itself only
    // exists on a patched servo tree (0009 introduces it), so the assignment
    // is feature-gated: without `patched-servo` the crate compiles against
    // the stock pinned rev and the patch series stays fully optional.
    #[cfg(feature = "patched-servo")]
    {
      preferences.layout_svg_native_enabled = true;
    }
    // The async Clipboard API (`navigator.clipboard`) is likewise implemented
    // behind a pref, and servo's default `clipboard` feature ships a system
    // clipboard delegate on desktop platforms — enabling it lights up copy
    // buttons with no embedder-side plumbing.
    preferences.dom_async_clipboard_enabled = true;
    // Two more implemented-but-gated APIs commonly probed by app frameworks
    // and editor libraries; both are standard and inert unless used.
    preferences.dom_intersection_observer_enabled = true;
    preferences.dom_composition_event_enabled = true;
    // Variable fonts: off by default, but the full pipeline exists (CSS
    // font-weight/-width/-optical-sizing composed into fvar coordinates,
    // applied to shaping, metrics, and rasterization). Without it a variable
    // TTF renders at its *default* fvar instance — for fonts whose default is
    // the Thin master (wght min), every weight renders hairline-thin with the
    // Thin master's spacing.
    preferences.layout_variable_fonts_enabled = true;

    // Schemes are registered with the (shared) engine once, by the first webview; every webview's
    // handlers go into the route table (provisionally until its Servo id exists, see below).
    let mut protocol_registry = ProtocolRegistry::default();
    let mut routes: HashMap<String, ProtocolRoute> = HashMap::new();
    for (scheme, handler) in custom_protocols {
      protocol_registry
        .register(
          &scheme,
          RoutedProtocol {
            scheme: scheme.clone(),
          },
        )
        .map_err(|error| {
          Error::Servo(format!(
            "failed to register custom protocol {scheme}: {error:?}"
          ))
        })?;
      routes.insert(scheme, (webview_id.clone(), std::sync::Arc::new(handler)));
    }
    if let Ok(mut table) = ROUTES.lock() {
      table.set_provisional(routes);
    }

    // The engine is created by the first webview, so preferences, waker and protocol registry
    // come from it. `hard_fail` is off: a panic in one page's thread must show about:failure for
    // that tab instead of exiting the whole process (Servo's default).
    let servo = SHARED_SERVO.with(|cell| {
      let mut slot = cell.borrow_mut();
      if let Some(existing) = slot.as_ref() {
        return existing.clone();
      }
      let mut opts = servo::Opts::default();
      opts.hard_fail = super::options::engine_options().hard_fail;
      let servo = Rc::new(
        ServoBuilder::default()
          .opts(opts)
          .preferences(preferences)
          .event_loop_waker(Box::new(waker))
          .protocol_registry(protocol_registry)
          .build(),
      );
      servo.setup_logging();
      *slot = Some(servo.clone());
      servo
    });

    let user_content_manager = Rc::new(UserContentManager::new(&servo));
    if delegate.ipc_handler.is_some() {
      user_content_manager.add_script(Rc::new(UserScript::from(IPC_BRIDGE_SCRIPT)));
    }
    for initialization_script in initialization_scripts {
      user_content_manager.add_script(Rc::new(UserScript::from(initialization_script.script)));
    }
    let webview_builder = ServoWebViewBuilder::new(&servo, target.rendering_context())
      .hidpi_scale_factor(Scale::new(target.scale_factor() as f32))
      .delegate(delegate.clone())
      .user_content_manager(user_content_manager);
    let webview = match initial_headers {
      Some(headers) => {
        let webview = webview_builder.build();
        webview.load_request(UrlRequest::new(initial_url).headers(headers));
        webview
      }
      None => webview_builder.url(initial_url).build(),
    };
    // The webview now has its Servo id: its provisional routes become its own.
    if let Ok(mut table) = ROUTES.lock() {
      table.promote(webview.id());
    }

    servo.spin_event_loop();

    BY_LABEL.with(|m| m.borrow_mut().insert(delegate.label.clone(), webview.clone()));
    LIVE.with(|live| live.borrow_mut().acquire());
    Ok(Self {
      servo,
      webview,
      target,
      delegate,
      cursor_position: Cell::new(None),
      modifiers: Cell::new(Modifiers::empty()),
      pending_ime_text: RefCell::new(None),
      pending_key_text: RefCell::new(None),
      pending_key_event: Cell::new(false),
      focused: Cell::new(false),
    })
  }

  /// Navigates, deferring while the initial load is in flight (see `NavGate`).
  pub(crate) fn load(&self, url: Url) {
    let action = self
      .delegate
      .nav_gate
      .borrow_mut()
      .request(url, std::time::Instant::now());
    if let NavAction::LoadNow(url) = action {
      self.webview.load(url);
    }
    self.servo.spin_event_loop();
  }

  pub fn handle_user_event(&self) {
    if let Some(url) = self
      .delegate
      .nav_gate
      .borrow_mut()
      .tick(std::time::Instant::now())
    {
      self.webview.load(url);
    }
    if let Some(text) = self.pending_ime_text.borrow_mut().take() {
      self.commit_ime_text(text);
    }
    self.servo.spin_event_loop();
    if matches!(self.target, RenderingTarget::Child { .. })
      && self.delegate.frame_ready.replace(false)
    {
      self.target.paint(&self.webview);
    }
  }

  pub fn set_control_flow(&self, control_flow: &mut ControlFlow) {
    *control_flow = if self.webview.animating() {
      ControlFlow::Poll
    } else {
      ControlFlow::Wait
    };
  }

  pub fn is_animating(&self) -> bool {
    self.webview.animating()
  }

  fn commit_ime_text(&self, text: String) {
    self
      .webview
      .notify_input_event(InputEvent::Ime(ImeEvent::Composition(CompositionEvent {
        state: CompositionState::End,
        data: text,
      })));
  }

  pub fn handle_window_event(&self, event: &WindowEvent<'_>) {
    self.servo.spin_event_loop();

    match event {
      WindowEvent::Resized(size) => self.target.handle_parent_resized(*size, &self.webview),
      WindowEvent::ScaleFactorChanged {
        scale_factor,
        new_inner_size,
      } => self
        .target
        .handle_scale_factor_changed(*scale_factor, **new_inner_size, &self.webview),
      WindowEvent::Focused(focused) => {
        if *focused && self.target.is_window() {
          self.focus_webview();
        } else if !focused {
          self.pending_ime_text.borrow_mut().take();
          self.pending_key_text.borrow_mut().take();
          self.pending_key_event.set(false);
          self.blur_webview();
        }
      }
      WindowEvent::ModifiersChanged(modifiers) => {
        self.modifiers.set(servo_modifiers(*modifiers));
      }
      WindowEvent::KeyboardInput { event, .. } if self.focused.get() => {
        // While a popup is open its navigation keys go to the app, not the page.
        if let Some(name) = popup_key_name(&event.logical_key).filter(|n| popup_wants_key(n)) {
          {
            if event.state == ElementState::Pressed && !event.repeat {
              notify_control(ControlEvent::Key(name));
            }
            self.servo.spin_event_loop();
            return;
          }
        }
        let state = match event.state {
          ElementState::Pressed => KeyState::Down,
          ElementState::Released => KeyState::Up,
          _ => {
            self.servo.spin_event_loop();
            return;
          }
        };
        if state == KeyState::Down {
          let key_text = inserted_key_text(&event.logical_key, event.text, self.modifiers.get());
          if let Some(ime_text) = self.pending_ime_text.borrow_mut().take() {
            if key_text.as_deref() != Some(ime_text.as_str()) {
              self.commit_ime_text(ime_text);
            }
            self.pending_key_text.borrow_mut().take();
            self.pending_key_event.set(false);
          } else {
            *self.pending_key_text.borrow_mut() = key_text;
            self.pending_key_event.set(true);
          }
        } else {
          self.pending_key_text.borrow_mut().take();
          self.pending_key_event.set(false);
        }
        self
          .webview
          .notify_input_event(InputEvent::Keyboard(KeyboardEvent::new_without_event(
            state,
            servo_key(&event.logical_key),
            servo_code(event.physical_key),
            servo_location(event.location),
            self.modifiers.get(),
            event.repeat,
            false,
          )));
      }
      WindowEvent::ReceivedImeText(text) if self.focused.get() => {
        if self.pending_key_event.replace(false) {
          if self.pending_key_text.borrow_mut().take().as_deref() != Some(text.as_str()) {
            self.commit_ime_text(text.clone());
          }
        } else {
          if let Some(previous) = self.pending_ime_text.borrow_mut().replace(text.clone()) {
            self.commit_ime_text(previous);
          }
          self.delegate.waker.wake();
        }
      }
      WindowEvent::CursorMoved { position, .. } => {
        let previous_position = self.cursor_position.get();
        let position = self.target.webview_point(*position);
        self.cursor_position.set(position);
        if let Some(p) = position {
          LAST_CURSOR.with(|c| c.set((p.x, p.y)));
        }
        match (previous_position, position) {
          (_, Some(point)) => {
            self
              .webview
              .notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(point.into())));
          }
          (Some(_), None) => {
            self
              .webview
              .notify_input_event(InputEvent::MouseLeftViewport(
                MouseLeftViewportEvent::default(),
              ));
          }
          (None, None) => {}
        }
      }
      WindowEvent::CursorLeft { .. } if self.cursor_position.replace(None).is_some() => {
        self
          .webview
          .notify_input_event(InputEvent::MouseLeftViewport(
            MouseLeftViewportEvent::default(),
          ));
      }
      WindowEvent::MouseInput { state, button, .. } => {
        let Some(point) = self.cursor_position.get() else {
          if *state == ElementState::Pressed {
            self.blur_webview();
            self.dismiss_overlay();
          }
          self.servo.spin_event_loop();
          return;
        };
        if *state == ElementState::Pressed {
          self.focus_webview();
        }
        let action = match state {
          ElementState::Pressed => MouseButtonAction::Down,
          ElementState::Released => MouseButtonAction::Up,
          _ => {
            self.servo.spin_event_loop();
            return;
          }
        };
        let Some(button) = servo_mouse_button(button) else {
          self.servo.spin_event_loop();
          return;
        };
        self
          .webview
          .notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
            action,
            button,
            point.into(),
          )));
      }
      WindowEvent::MouseWheel { delta, .. } => {
        let Some(point) = self.cursor_position.get() else {
          self.servo.spin_event_loop();
          return;
        };
        let Some(delta) = servo_wheel_delta(delta) else {
          self.servo.spin_event_loop();
          return;
        };
        self
          .webview
          .notify_input_event(InputEvent::Wheel(WheelEvent::new(delta, point.into())));
      }
      WindowEvent::Touch(touch) => {
        let Some(point) = self.target.webview_point(touch.location) else {
          self.servo.spin_event_loop();
          return;
        };
        let Some(event_type) = servo_touch_phase(touch.phase) else {
          self.servo.spin_event_loop();
          return;
        };
        if touch.phase == TouchPhase::Started {
          self.focus_webview();
        }
        self
          .webview
          .notify_input_event(InputEvent::Touch(TouchEvent::new(
            event_type,
            TouchId(touch.id as i32),
            point.into(),
            TouchPointerType::Touch,
          )));
      }
      _ => {}
    }

    self.servo.spin_event_loop();
  }

  pub fn paint(&self) {
    self.servo.spin_event_loop();
    self.delegate.frame_ready.set(false);
    self.target.paint(&self.webview);
  }

  pub fn is_shutdown(&self) -> bool {
    self.delegate.closed.get()
  }

  pub(crate) fn notify_theme_change(&self, theme: ServoTheme) {
    self.webview.notify_theme_change(theme);
    self.servo.spin_event_loop();
  }

  pub(crate) fn set_background_color(&self, background_color: [f64; 4]) {
    self
      .servo
      .set_preference("shell_background_color_rgba", background_color.into());
    self.delegate.request_repaint();
    self.servo.spin_event_loop();
  }

  pub(crate) fn bounds(&self) -> Rect {
    self.target.bounds()
  }

  pub(crate) fn set_bounds(&self, bounds: Rect) {
    self.target.set_bounds(bounds, &self.webview);
    self.delegate.request_repaint();
    self.servo.spin_event_loop();
  }

  pub(crate) fn set_visible(&self, visible: bool) {
    self.target.set_window_visible(visible);
    // A hidden child leaves the draw order and stops receiving input; the repaint recomposes the
    // window so its pixels disappear (or reappear) right away.
    self.target.set_child_visible(visible);
    if visible {
      self.webview.show();
    } else {
      self.blur_webview();
      self.webview.hide();
    }
    self.delegate.request_repaint();
    self.servo.spin_event_loop();
  }

  pub(crate) fn focus(&self) {
    let _ = self.target.focus_parent();
    self.focus_webview();
    self.servo.spin_event_loop();
  }

  fn focus_webview(&self) {
    if !self.focused.replace(true) {
      self.webview.focus();
    }
  }

  /// A mouse press landed outside this webview while it owns the window's overlay region: clear the
  /// overlay, redraw, and tell the app (see `set_overlay_dismiss_handler`).
  fn dismiss_overlay(&self) {
    let RenderingTarget::Child {
      window_id,
      child_id,
      parent_context,
      ..
    } = &self.target
    else {
      return;
    };
    let cleared = with_compositor(*window_id, |compositor| match compositor.overlay() {
      Some(overlay) if overlay.id == *child_id => {
        compositor.set_overlay(None);
        true
      }
      _ => false,
    })
    .unwrap_or(false);
    if !cleared {
      return;
    }
    composite_children(*window_id, parent_context);
    parent_context.present();
    let handler = OVERLAY_DISMISS_HANDLER.lock().ok().and_then(|slot| slot.clone());
    if let Some(handler) = handler {
      handler();
    }
  }

  fn blur_webview(&self) {
    if self.focused.replace(false) {
      self.webview.blur();
    }
  }

  pub(crate) fn focus_parent(&self) -> Result<()> {
    self.target.focus_parent()
  }

  pub(crate) fn servo(&self) -> &Servo {
    &self.servo
  }

  pub(crate) fn webview(&self) -> &ServoWebView {
    &self.webview
  }
}

#[cfg(test)]
mod tests {
  use super::ipc_request_uri;
  use std::cell::RefCell;
  use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
  };

  use crate::servo::nav_gate::NavGate;

  use dpi::{LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize};
  use keyboard_types::{Code, Key, Modifiers, NamedKey};
  use tao::{
    event::{MouseButton as TaoMouseButton, MouseScrollDelta, TouchPhase},
    keyboard::{Key as TaoKey, KeyCode as TaoKeyCode, KeyLocation, ModifiersState},
  };

  use super::{
    inserted_key_text, ipc_message_body, non_zero_size, point_in_bounds, servo_code, servo_key,
    servo_location, servo_modifiers, servo_mouse_button, servo_touch_phase, servo_wheel_delta,
    Delegate, EmbedderWaker, PhysicalBounds, IPC_BRIDGE_SCRIPT, IPC_MESSAGE_PREFIX,
  };
  use crate::Rect;

  #[test]
  fn recognizes_ipc_console_messages() {
    assert_eq!(ipc_message_body("__SERVO_IPC__:hello"), Some("hello"));
    assert_eq!(ipc_message_body("ordinary console message"), None);
  }

  #[test]
  fn ipc_bridge_uses_the_embedder_prefix() {
    assert!(IPC_BRIDGE_SCRIPT.contains(IPC_MESSAGE_PREFIX));
  }

  #[test]
  fn converts_child_bounds_to_physical_pixels() {
    let bounds = PhysicalBounds::from_rect(
      Rect {
        position: LogicalPosition::new(10, 20).into(),
        size: LogicalSize::new(300, 200).into(),
      },
      2.0,
    );

    assert_eq!(bounds.position, PhysicalPosition::new(20, 40));
    assert_eq!(bounds.size, PhysicalSize::new(600, 400));
  }

  #[test]
  fn clamps_zero_child_dimensions_for_servo() {
    let bounds = PhysicalBounds::from_rect(
      Rect {
        position: PhysicalPosition::new(0, 0).into(),
        size: PhysicalSize::new(0, 0).into(),
      },
      1.0,
    );

    assert_eq!(bounds.size, PhysicalSize::new(1, 1));
  }

  #[test]
  fn converts_parent_coordinates_to_child_device_coordinates() {
    assert_eq!(
      point_in_bounds(
        PhysicalPosition::new(125.0, 90.0),
        PhysicalPosition::new(100, 50),
        PhysicalSize::new(200, 100),
      ),
      Some(servo::DevicePoint::new(25.0, 40.0))
    );
    assert_eq!(
      point_in_bounds(
        PhysicalPosition::new(99.0, 90.0),
        PhysicalPosition::new(100, 50),
        PhysicalSize::new(200, 100),
      ),
      None
    );
  }

  #[test]
  fn converts_tao_keyboard_values_to_dom_values() {
    assert_eq!(
      servo_key(&TaoKey::Character("x")),
      Key::Character("x".into())
    );
    assert_eq!(servo_key(&TaoKey::Enter), Key::Named(NamedKey::Enter));
    assert_eq!(servo_key(&TaoKey::Space), Key::Character(" ".into()));
    assert_eq!(servo_key(&TaoKey::Super), Key::Named(NamedKey::Meta));
    assert_eq!(servo_code(TaoKeyCode::KeyA), Code::KeyA);
    assert_eq!(servo_code(TaoKeyCode::SuperLeft), Code::MetaLeft);
  }

  #[test]
  fn converts_tao_modifier_state() {
    let modifiers = servo_modifiers(ModifiersState::SHIFT | ModifiersState::SUPER);
    assert!(modifiers.contains(Modifiers::SHIFT));
    assert!(modifiers.contains(Modifiers::META));
    assert!(!modifiers.contains(Modifiers::CONTROL));
  }

  #[test]
  fn converts_tao_input_variants_to_servo_values() {
    assert!(matches!(
      servo_mouse_button(&TaoMouseButton::Left),
      Some(servo::MouseButton::Left)
    ));
    assert!(matches!(
      servo_mouse_button(&TaoMouseButton::Other(4)),
      Some(servo::MouseButton::Other(4))
    ));
    assert_eq!(
      servo_location(KeyLocation::Numpad),
      keyboard_types::Location::Numpad
    );
    assert!(matches!(
      servo_touch_phase(TouchPhase::Started),
      Some(servo::TouchEventType::Down)
    ));
    assert!(matches!(
      servo_touch_phase(TouchPhase::Cancelled),
      Some(servo::TouchEventType::Cancel)
    ));
  }

  #[test]
  fn converts_line_and_pixel_wheel_deltas() {
    let line = servo_wheel_delta(&MouseScrollDelta::LineDelta(1.5, -2.0)).unwrap();
    assert_eq!((line.x, line.y, line.z), (114.0, -152.0, 0.0));
    assert_eq!(line.mode, servo::WheelMode::DeltaPixel);

    let pixel = servo_wheel_delta(&MouseScrollDelta::PixelDelta(PhysicalPosition::new(
      3.25, -4.5,
    )))
    .unwrap();
    assert_eq!((pixel.x, pixel.y, pixel.z), (3.25, -4.5, 0.0));
    assert_eq!(pixel.mode, servo::WheelMode::DeltaPixel);
  }

  #[test]
  fn clamps_each_zero_dimension() {
    assert_eq!(
      non_zero_size(PhysicalSize::new(0, 5)),
      PhysicalSize::new(1, 5)
    );
    assert_eq!(
      non_zero_size(PhysicalSize::new(7, 0)),
      PhysicalSize::new(7, 1)
    );
  }

  #[test]
  fn repainting_an_offscreen_webview_marks_the_frame_and_wakes_the_host() {
    let wake_count = Arc::new(AtomicUsize::new(0));
    let wake_count_ = wake_count.clone();
    let delegate = Delegate {
      window: None,
      waker: EmbedderWaker::new(move || {
        wake_count_.fetch_add(1, Ordering::Relaxed);
      }),
      label: String::new(),
      nav_gate: RefCell::new(NavGate::new(
        std::time::Instant::now(),
        std::time::Duration::from_secs(2),
      )),
      frame_ready: Default::default(),
      closed: Default::default(),
      ipc_handler: None,
      navigation_handler: None,
      document_title_changed_handler: None,
      on_page_load_handler: None,
    };

    delegate.request_repaint();

    assert!(delegate.frame_ready.get());
    assert_eq!(wake_count.load(Ordering::Relaxed), 1);
  }

  #[test]
  fn identifies_text_already_inserted_by_a_keydown() {
    assert_eq!(
      inserted_key_text(&TaoKey::Character("A"), Some("A"), Modifiers::SHIFT),
      Some("A".into())
    );
    assert_eq!(
      inserted_key_text(&TaoKey::Character("c"), Some("c"), Modifiers::META),
      None
    );
    assert_eq!(
      inserted_key_text(&TaoKey::Process, None, Modifiers::empty()),
      None
    );
    assert_eq!(
      inserted_key_text(&TaoKey::Dead(Some('´')), Some("é"), Modifiers::empty()),
      Some("é".into())
    );
  }

  #[test]
  fn ipc_uri_is_the_page_url_when_valid() {
    assert_eq!(ipc_request_uri(Some("https://example.com/a?b=1")).to_string(), "https://example.com/a?b=1");
    assert_eq!(ipc_request_uri(Some("tauri://localhost/")).to_string(), "tauri://localhost/");
  }

  #[test]
  fn ipc_uri_for_non_hierarchical_or_missing_urls_is_a_parseable_remote_origin() {
    for url in [Some("data:text/html,hi"), Some("blob:https://a.com/uuid"), Some("about:blank"), Some(""), None] {
      let uri = ipc_request_uri(url).to_string();
      assert!(url::Url::parse(&uri).is_ok(), "{url:?} -> {uri} must parse as an absolute URL");
      assert!(uri.starts_with("http://"), "{url:?} -> {uri} must be attributed to a remote origin");
    }
  }
}

#[cfg(test)]
mod select_tests {
  use super::*;
  use servo::{SelectElementOption, SelectElementOptionOrOptgroup as E};

  fn opt(id: usize, label: &str, disabled: bool) -> SelectElementOption {
    SelectElementOption { id, label: label.into(), is_disabled: disabled }
  }

  #[test]
  fn options_and_groups_flatten_into_rows() {
    let rows = flatten_select_options(&[
      E::Option(opt(0, "One", false)),
      E::Optgroup { label: "Group".into(), options: vec![opt(1, "Two", false), opt(2, "Three", true)] },
    ]);
    assert_eq!(rows.len(), 4);
    assert!(!rows[0].group && !rows[0].indented && rows[0].id == 0);
    assert!(rows[1].group && rows[1].disabled && rows[1].label == "Group");
    assert!(rows[2].indented && rows[2].id == 1);
    assert!(rows[3].disabled && rows[3].id == 2);
  }
}

#[cfg(test)]
mod popup_key_tests {
  use super::*;

  #[test]
  fn only_navigation_keys_drive_popups() {
    assert_eq!(popup_key_name(&TaoKey::ArrowDown), Some("ArrowDown"));
    assert_eq!(popup_key_name(&TaoKey::Enter), Some("Enter"));
    assert_eq!(popup_key_name(&TaoKey::Escape), Some("Escape"));
    assert_eq!(popup_key_name(&TaoKey::Character("a")), None);
    assert_eq!(popup_key_name(&TaoKey::Space), None);
  }
}
