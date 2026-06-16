// Native GDI reminder windows. These do not depend on the main WebView, so they
// still appear when Gaze20 is minimized to the tray.

use std::ffi::c_void;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Runtime};
use windows::core::{w, BOOL, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    Arc, BeginPaint, CreateFontW, CreatePen, CreateSolidBrush, DeleteObject, DrawTextW, Ellipse,
    EndPaint, EnumDisplayMonitors, FillRect, GetMonitorInfoW, GetStockObject, InvalidateRect,
    LineTo, MoveToEx, Polygon, RoundRect, SelectObject, SetBkMode, SetTextColor, CLIP_DEFAULT_PRECIS,
    DEFAULT_CHARSET, DEFAULT_PITCH, DEFAULT_QUALITY, DT_CENTER, DT_SINGLELINE, DT_VCENTER,
    DT_WORDBREAK, FW_BOLD, FW_MEDIUM, FW_NORMAL, HBRUSH, HDC, HGDIOBJ, HMONITOR, MONITORINFO,
    NULL_BRUSH, OUT_DEFAULT_PRECIS, PAINTSTRUCT, PS_SOLID, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW,
    GetWindowLongPtrW, KillTimer, LoadCursorW, PostQuitMessage, PostThreadMessageW,
    RegisterClassW, SetTimer, SetWindowLongPtrW, ShowWindow, TranslateMessage, CREATESTRUCTW,
    CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA, IDC_ARROW, MSG, SW_SHOW, WM_APP, WM_CLOSE,
    WM_LBUTTONUP, WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WM_TIMER, WNDCLASSW, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};

const CLASS_NAME: PCWSTR = w!("Gaze20NativeReminder");
const WINDOW_TITLE: PCWSTR = w!("Gaze20 Reminder");
const TIMER_ID: usize = 20;
const WM_NATIVE_ACTION: u32 = WM_APP + 0x201;
const WM_NATIVE_CLOSE: u32 = WM_APP + 0x202;
const WM_NATIVE_START_REQUEST: u32 = WM_APP + 0x203;
const WM_NATIVE_STARTED: u32 = WM_APP + 0x204;
const WM_NATIVE_TICK: u32 = WM_APP + 0x205;
const ACTION_COMPLETE: usize = 1;
const ACTION_POSTPONE: usize = 2;
const ACTION_SKIP: usize = 3;

static SESSION: OnceLock<Mutex<Option<NativeSession>>> = OnceLock::new();

struct NativeSession {
    thread_id: u32,
}

#[derive(Clone)]
struct ReminderConfig {
    kind: ReminderKind,
    seconds: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReminderKind {
    Micro,
    Deep,
}

struct UiState {
    config: ReminderConfig,
    remaining: u32,
    started: bool,
    timer_hwnd: Option<HWND>,
    thread_id: u32,
    windows: Vec<HWND>,
}

struct WindowContext {
    state: Arc<Mutex<UiState>>,
}

enum Startup {
    Ready { thread_id: u32, window_count: usize },
    Failed(String),
}

#[derive(Clone, Copy)]
struct Layout {
    card: RECT,
    header: RECT,
    countdown: RECT,
    tip_left: RECT,
    tip_right: RECT,
    privacy: RECT,
    monitor_note: RECT,
    close: RECT,
    primary: RECT,
    postpone: RECT,
    skip: RECT,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReminderSyncPayload {
    remaining: u32,
    started: bool,
}

pub fn show<R: Runtime + 'static>(
    app: AppHandle<R>,
    kind: String,
    seconds: u32,
    _image_index: u8,
    _score: u32,
) -> Result<u32, String> {
    close();

    let config = ReminderConfig {
        kind: if kind == "deep" {
            ReminderKind::Deep
        } else {
            ReminderKind::Micro
        },
        seconds: seconds.max(1),
    };

    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("gaze20-native-reminder".to_string())
        .spawn(move || {
            unsafe { run_reminder_thread(app, config, tx) };
        })
        .map_err(|error| error.to_string())?;

    match rx
        .recv_timeout(Duration::from_secs(2))
        .map_err(|error| format!("无法创建原生提醒窗口: {error}"))?
    {
        Startup::Ready {
            thread_id,
            window_count,
        } => {
            if window_count == 0 {
                return Ok(0);
            }
            let mut session = session_slot().lock().map_err(|error| error.to_string())?;
            *session = Some(NativeSession { thread_id });
            Ok(window_count as u32)
        }
        Startup::Failed(error) => Err(error),
    }
}

pub fn close() {
    let Some(slot) = SESSION.get() else {
        return;
    };
    let Ok(mut guard) = slot.lock() else {
        return;
    };
    if let Some(session) = guard.take() {
        unsafe {
            let _ = PostThreadMessageW(
                session.thread_id,
                WM_NATIVE_CLOSE,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

pub fn start() -> bool {
    let Some(slot) = SESSION.get() else {
        return false;
    };
    let Ok(guard) = slot.lock() else {
        return false;
    };
    let Some(session) = guard.as_ref() else {
        return false;
    };
    unsafe {
        PostThreadMessageW(
            session.thread_id,
            WM_NATIVE_START_REQUEST,
            WPARAM(0),
            LPARAM(0),
        )
        .is_ok()
    }
}

fn session_slot() -> &'static Mutex<Option<NativeSession>> {
    SESSION.get_or_init(|| Mutex::new(None))
}

unsafe fn run_reminder_thread<R: Runtime + 'static>(
    app: AppHandle<R>,
    config: ReminderConfig,
    startup_tx: mpsc::Sender<Startup>,
) {
    let thread_id = GetCurrentThreadId();
    if let Err(error) = register_window_class() {
        let _ = startup_tx.send(Startup::Failed(error));
        return;
    }

    let monitors = collect_monitors();
    if monitors.is_empty() {
        let _ = startup_tx.send(Startup::Ready {
            thread_id,
            window_count: 0,
        });
        return;
    }

    let state = Arc::new(Mutex::new(UiState {
        remaining: config.seconds,
        config,
        started: false,
        timer_hwnd: None,
        thread_id,
        windows: Vec::new(),
    }));

    for monitor in monitors {
        match create_reminder_window(monitor, state.clone()) {
            Ok(hwnd) => {
                if let Ok(mut guard) = state.lock() {
                    guard.windows.push(hwnd);
                }
                let _ = ShowWindow(hwnd, SW_SHOW);
                let _ = InvalidateRect(Some(hwnd), None, true);
            }
            Err(error) => {
                let _ = startup_tx.send(Startup::Failed(error));
                destroy_all(&state);
                return;
            }
        }
    }

    let window_count = state.lock().map(|guard| guard.windows.len()).unwrap_or(0);
    let _ = startup_tx.send(Startup::Ready {
        thread_id,
        window_count,
    });
    message_loop(app, state);
}

unsafe fn message_loop<R: Runtime + 'static>(
    app: AppHandle<R>,
    state: Arc<Mutex<UiState>>,
) {
    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        if msg.message == WM_NATIVE_CLOSE {
            destroy_all(&state);
            break;
        }

        if msg.message == WM_NATIVE_START_REQUEST {
            start_from_app(&state);
            continue;
        }

        if msg.message == WM_NATIVE_STARTED {
            let _ = app.emit(
                "overlay-start",
                ReminderSyncPayload {
                    remaining: msg.wParam.0 as u32,
                    started: true,
                },
            );
            continue;
        }

        if msg.message == WM_NATIVE_TICK {
            let _ = app.emit(
                "overlay-tick",
                ReminderSyncPayload {
                    remaining: msg.wParam.0 as u32,
                    started: msg.lParam.0 != 0,
                },
            );
            continue;
        }

        if msg.message == WM_NATIVE_ACTION {
            let action = match msg.wParam.0 {
                ACTION_POSTPONE => "postpone",
                ACTION_SKIP => "skip",
                _ => "complete",
            };
            destroy_all(&state);
            let _ = app.emit("overlay-action", action);
            break;
        }

        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    destroy_all(&state);
}

unsafe fn register_window_class() -> Result<(), String> {
    let module = GetModuleHandleW(PCWSTR::null()).map_err(|error| error.to_string())?;
    let instance: HINSTANCE = module.into();
    let cursor = LoadCursorW(None, IDC_ARROW).map_err(|error| error.to_string())?;
    let class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wnd_proc),
        hInstance: instance,
        hCursor: cursor,
        lpszClassName: CLASS_NAME,
        ..Default::default()
    };
    RegisterClassW(&class);
    Ok(())
}

unsafe fn collect_monitors() -> Vec<RECT> {
    let mut monitors = Vec::<RECT>::new();
    let data = LPARAM((&mut monitors as *mut Vec<RECT>) as isize);
    let _ = EnumDisplayMonitors(None, None, Some(enum_monitor), data);
    monitors
}

unsafe extern "system" fn enum_monitor(
    monitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let monitors = &mut *(data.0 as *mut Vec<RECT>);
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(monitor, &mut info).as_bool() {
        monitors.push(info.rcMonitor);
    }
    BOOL(1)
}

unsafe fn create_reminder_window(
    monitor: RECT,
    state: Arc<Mutex<UiState>>,
) -> Result<HWND, String> {
    let width = (monitor.right - monitor.left).max(360);
    let height = (monitor.bottom - monitor.top).max(520);
    let x = monitor.left;
    let y = monitor.top;
    let module = GetModuleHandleW(PCWSTR::null()).map_err(|error| error.to_string())?;
    let instance: HINSTANCE = module.into();
    let context = Box::new(WindowContext { state });
    let raw_context = Box::into_raw(context);

    match CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
        CLASS_NAME,
        WINDOW_TITLE,
        WS_POPUP | WS_VISIBLE,
        x,
        y,
        width,
        height,
        None,
        None,
        Some(instance),
        Some(raw_context.cast::<c_void>() as *const c_void),
    ) {
        Ok(hwnd) => Ok(hwnd),
        Err(error) => {
            drop(Box::from_raw(raw_context));
            Err(error.to_string())
        }
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = lparam.0 as *const CREATESTRUCTW;
        if !create.is_null() {
            let context = (*create).lpCreateParams as *mut WindowContext;
            if !context.is_null() {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, context as isize);
                return LRESULT(1);
            }
        }
        return LRESULT(0);
    }

    let context = window_context(hwnd);

    match message {
        WM_PAINT => {
            if let Some(context) = context {
                paint_window(hwnd, context);
                return LRESULT(0);
            }
        }
        WM_LBUTTONUP => {
            if let Some(context) = context {
                let x = (lparam.0 & 0xffff) as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xffff) as i16 as i32;
                handle_click(hwnd, context, x, y);
                return LRESULT(0);
            }
        }
        WM_TIMER => {
            if wparam.0 == TIMER_ID {
                if let Some(context) = context {
                    handle_timer(hwnd, context);
                    return LRESULT(0);
                }
            }
        }
        WM_CLOSE => {
            if let Some(context) = context {
                post_action(context, ACTION_SKIP);
                return LRESULT(0);
            }
        }
        WM_NCDESTROY => {
            if let Some(ptr) = take_window_context(hwnd) {
                drop(Box::from_raw(ptr));
            }
            return DefWindowProcW(hwnd, message, wparam, lparam);
        }
        _ => {}
    }

    DefWindowProcW(hwnd, message, wparam, lparam)
}

unsafe fn window_context(hwnd: HWND) -> Option<&'static WindowContext> {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowContext;
    ptr.as_ref()
}

unsafe fn take_window_context(hwnd: HWND) -> Option<*mut WindowContext> {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowContext;
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
    if ptr.is_null() {
        None
    } else {
        Some(ptr)
    }
}

unsafe fn handle_click(hwnd: HWND, context: &WindowContext, x: i32, y: i32) {
    let mut client = RECT::default();
    if GetClientRect(hwnd, &mut client).is_err() {
        return;
    }
    let layout = layout(client.right - client.left, client.bottom - client.top);
    if point_in_rect(x, y, layout.close) || point_in_rect(x, y, layout.skip) {
        post_action(context, ACTION_SKIP);
        return;
    }
    if point_in_rect(x, y, layout.postpone) {
        post_action(context, ACTION_POSTPONE);
        return;
    }
    if !point_in_rect(x, y, layout.primary) {
        return;
    }

    let mut should_complete = false;
    if let Ok(mut state) = context.state.lock() {
        if state.started {
            should_complete = true;
        } else {
            start_locked(&mut state, hwnd);
        }
    }

    if should_complete {
        post_action(context, ACTION_COMPLETE);
    }
}

unsafe fn start_from_app(state: &Arc<Mutex<UiState>>) {
    if let Ok(mut guard) = state.lock() {
        let hwnd = match guard.timer_hwnd.or_else(|| guard.windows.first().copied()) {
            Some(hwnd) => hwnd,
            None => return,
        };
        if !guard.started {
            start_locked(&mut guard, hwnd);
        } else {
            post_sync_message(&guard, WM_NATIVE_STARTED);
        }
    }
}

unsafe fn start_locked(state: &mut UiState, hwnd: HWND) {
    state.started = true;
    state.timer_hwnd = Some(hwnd);
    state.remaining = state.config.seconds;
    SetTimer(Some(hwnd), TIMER_ID, 1000, None);
    invalidate_all_locked(state);
    post_sync_message(state, WM_NATIVE_STARTED);
}

unsafe fn handle_timer(hwnd: HWND, context: &WindowContext) {
    let mut should_complete = false;
    if let Ok(mut state) = context.state.lock() {
        if state.timer_hwnd != Some(hwnd) || !state.started {
            return;
        }
        state.remaining = state.remaining.saturating_sub(1);
        invalidate_all_locked(&state);
        post_sync_message(&state, WM_NATIVE_TICK);
        if state.remaining == 0 {
            let _ = KillTimer(Some(hwnd), TIMER_ID);
            should_complete = true;
        }
    }

    if should_complete {
        post_action(context, ACTION_COMPLETE);
    }
}

unsafe fn post_sync_message(state: &UiState, message: u32) {
    let _ = PostThreadMessageW(
        state.thread_id,
        message,
        WPARAM(state.remaining as usize),
        LPARAM(if state.started { 1 } else { 0 }),
    );
}

unsafe fn post_action(context: &WindowContext, action: usize) {
    if let Ok(state) = context.state.lock() {
        let _ = PostThreadMessageW(state.thread_id, WM_NATIVE_ACTION, WPARAM(action), LPARAM(0));
    }
}

unsafe fn destroy_all(state: &Arc<Mutex<UiState>>) {
    let windows = {
        let Ok(mut guard) = state.lock() else {
            return;
        };
        if let Some(hwnd) = guard.timer_hwnd.take() {
            let _ = KillTimer(Some(hwnd), TIMER_ID);
        }
        std::mem::take(&mut guard.windows)
    };

    for hwnd in windows {
        let _ = DestroyWindow(hwnd);
    }
    PostQuitMessage(0);
}

unsafe fn invalidate_all_locked(state: &UiState) {
    for hwnd in &state.windows {
        let _ = InvalidateRect(Some(*hwnd), None, true);
    }
}

fn point_in_rect(x: i32, y: i32, rect: RECT) -> bool {
    x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom
}

fn layout(width: i32, height: i32) -> Layout {
    let max_card_w = (width - 72).max(360);
    let max_card_h = (height - 124).max(520);
    let card_w = 640.min(max_card_w).max(420);
    let card_h = 860.min(max_card_h).max(620);
    let card_left = (width - card_w) / 2;
    let card_top = ((height - card_h) / 2 - 18).max(36);
    let card = RECT {
        left: card_left,
        top: card_top,
        right: card_left + card_w,
        bottom: card_top + card_h,
    };
    let header_h = (card_h * 34 / 100).clamp(210, 300);
    let header = RECT {
        left: card.left,
        top: card.top,
        right: card.right,
        bottom: card.top + header_h,
    };
    let countdown_size = (card_w * 27 / 100).clamp(150, 188);
    let countdown = RECT {
        left: (card.left + card.right - countdown_size) / 2,
        top: header.bottom - countdown_size / 2 - 22,
        right: (card.left + card.right + countdown_size) / 2,
        bottom: header.bottom + countdown_size / 2 - 22,
    };
    let close = RECT {
        left: card.right - 62,
        top: card.top + 28,
        right: card.right - 24,
        bottom: card.top + 66,
    };
    let tip_top = card.bottom - 274;
    let tip_w = (card_w - 96) / 2;
    let tip_left = RECT {
        left: card.left + 48,
        top: tip_top,
        right: card.left + 48 + tip_w,
        bottom: tip_top + 58,
    };
    let tip_right = RECT {
        left: card.right - 48 - tip_w,
        top: tip_top,
        right: card.right - 48,
        bottom: tip_top + 58,
    };
    let button_top = card.bottom - 184;
    let gap = 16;
    let primary_width = 208;
    let ghost_width = 156;
    let skip_width = 138;
    let total = primary_width + ghost_width + skip_width + gap * 2;
    let left = (card.left + card.right - total) / 2;
    let primary = RECT {
        left,
        top: button_top,
        right: left + primary_width,
        bottom: button_top + 62,
    };
    let postpone = RECT {
        left: primary.right + gap,
        top: button_top,
        right: primary.right + gap + ghost_width,
        bottom: button_top + 62,
    };
    let skip = RECT {
        left: postpone.right + gap,
        top: button_top,
        right: postpone.right + gap + skip_width,
        bottom: button_top + 62,
    };
    let privacy = RECT {
        left: card.left,
        top: card.bottom - 82,
        right: card.right,
        bottom: card.bottom - 44,
    };
    let monitor_note = RECT {
        left: 0,
        top: card.bottom + 34,
        right: width,
        bottom: card.bottom + 74,
    };
    Layout {
        card,
        header,
        countdown,
        tip_left,
        tip_right,
        privacy,
        monitor_note,
        close,
        primary,
        postpone,
        skip,
    }
}

unsafe fn paint_window(hwnd: HWND, context: &WindowContext) {
    let mut paint = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut paint);
    let mut client = RECT::default();
    let _ = GetClientRect(hwnd, &mut client);
    let width = client.right - client.left;
    let height = client.bottom - client.top;

    let snapshot = context.state.lock().map(|state| {
        (
            state.config.clone(),
            state.remaining,
            state.started,
            state.config.seconds,
        )
    });
    let (config, remaining, started, total_seconds) = match snapshot {
        Ok(value) => value,
        Err(_) => {
            let _ = EndPaint(hwnd, &paint);
            return;
        }
    };

    let ui = layout(width, height);
    fill(hdc, client, rgb(36, 62, 53));
    draw_round(
        hdc,
        RECT {
            left: ui.card.left - 2,
            top: ui.card.top - 2,
            right: ui.card.right + 2,
            bottom: ui.card.bottom + 2,
        },
        rgb(205, 226, 218),
        rgb(42, 72, 62),
        1,
        46,
    );
    draw_round(hdc, ui.card, rgb(238, 249, 245), rgb(211, 227, 221), 2, 44);
    paint_scene(hdc, ui);
    paint_close(hdc, ui.close);

    draw_text(
        hdc,
        "◉  远眺 Gaze20",
        RECT {
            left: ui.card.left,
            top: ui.card.top + 28,
            right: ui.card.right,
            bottom: ui.card.top + 64,
        },
        20,
        FW_MEDIUM.0 as i32,
        rgb(39, 64, 57),
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );

    paint_countdown(hdc, ui, remaining, started, total_seconds);

    let title = match config.kind {
        ReminderKind::Deep => "该起身深休息了",
        ReminderKind::Micro => "该远眺一下了",
    };
    let title_top = ui.countdown.bottom + 44;
    draw_text(
        hdc,
        title,
        RECT {
            left: ui.card.left + 52,
            top: title_top,
            right: ui.card.right - 52,
            bottom: title_top + 62,
        },
        40,
        FW_BOLD.0 as i32,
        rgb(26, 56, 48),
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );

    let detail = if config.kind == ReminderKind::Deep {
        "连续专注时间较长，起身活动一下，让眼睛和肩颈都放松"
    } else {
        "看向 6 米外，保持 20 秒，并做 5-10 次完整眨眼"
    };
    draw_text(
        hdc,
        "你已连续专注 24 分钟",
        RECT {
            left: ui.card.left + 52,
            top: title_top + 68,
            right: ui.card.right - 52,
            bottom: title_top + 102,
        },
        23,
        FW_MEDIUM.0 as i32,
        rgb(91, 124, 113),
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );
    draw_text(
        hdc,
        detail,
        RECT {
            left: ui.card.left + 48,
            top: title_top + 106,
            right: ui.card.right - 48,
            bottom: title_top + 148,
        },
        22,
        FW_NORMAL.0 as i32,
        rgb(91, 124, 113),
        DT_CENTER | DT_VCENTER | DT_WORDBREAK,
    );

    paint_tips(hdc, ui, config.kind);
    paint_buttons(hdc, ui, started);

    draw_text(
        hdc,
        "盾  仅本地提醒，不采集摄像头画面",
        ui.privacy,
        18,
        FW_NORMAL.0 as i32,
        rgb(134, 159, 150),
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );
    draw_text(
        hdc,
        "▣  提醒框会在每个显示器中央同时出现",
        ui.monitor_note,
        19,
        FW_MEDIUM.0 as i32,
        rgb(178, 196, 189),
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );

    let _ = EndPaint(hwnd, &paint);
}

unsafe fn paint_scene(hdc: HDC, ui: Layout) {
    let header = ui.header;
    draw_round(hdc, header, rgb(224, 242, 236), rgb(224, 242, 236), 1, 44);
    fill(
        hdc,
        RECT {
            left: header.left,
            top: header.top + 42,
            right: header.right,
            bottom: header.bottom,
        },
        rgb(224, 242, 236),
    );

    let w = header.right - header.left;
    let h = header.bottom - header.top;
    let x = header.left;
    let y = header.top;
    draw_ellipse(
        hdc,
        RECT {
            left: x + w - 190,
            top: y + 34,
            right: x + w - 82,
            bottom: y + 142,
        },
        rgb(198, 226, 217),
        rgb(198, 226, 217),
        1,
    );
    draw_ellipse(
        hdc,
        RECT {
            left: x + 58,
            top: y + 36,
            right: x + 108,
            bottom: y + 86,
        },
        rgb(212, 236, 229),
        rgb(212, 236, 229),
        1,
    );

    let base = y + h - 6;
    draw_polygon(
        hdc,
        &[
            (x, base - 92),
            (x + w * 13 / 100, base - 168),
            (x + w * 25 / 100, base - 108),
            (x + w * 37 / 100, base - 188),
            (x + w * 52 / 100, base - 112),
            (x + w * 65 / 100, base - 184),
            (x + w * 78 / 100, base - 108),
            (x + w * 92 / 100, base - 168),
            (x + w, base - 128),
            (x + w, base),
            (x, base),
        ],
        rgb(184, 218, 207),
    );
    draw_polygon(
        hdc,
        &[
            (x, base - 72),
            (x + w * 16 / 100, base - 126),
            (x + w * 30 / 100, base - 74),
            (x + w * 45 / 100, base - 144),
            (x + w * 61 / 100, base - 78),
            (x + w * 78 / 100, base - 140),
            (x + w, base - 82),
            (x + w, base),
            (x, base),
        ],
        rgb(154, 202, 184),
    );
    draw_polygon(
        hdc,
        &[
            (x, base - 36),
            (x + w * 20 / 100, base - 84),
            (x + w * 38 / 100, base - 46),
            (x + w * 55 / 100, base - 88),
            (x + w * 74 / 100, base - 48),
            (x + w, base - 72),
            (x + w, base),
            (x, base),
        ],
        rgb(127, 181, 160),
    );
    draw_polygon(hdc, &[(x + 124, base), (x + 138, base - 48), (x + 152, base)], rgb(101, 160, 139));
    draw_polygon(hdc, &[(x + w - 190, base), (x + w - 174, base - 48), (x + w - 158, base)], rgb(101, 160, 139));
}

unsafe fn paint_close(hdc: HDC, rect: RECT) {
    draw_line(
        hdc,
        rect.left + 10,
        rect.top + 10,
        rect.right - 10,
        rect.bottom - 10,
        rgb(78, 95, 103),
        3,
    );
    draw_line(
        hdc,
        rect.left + 10,
        rect.bottom - 10,
        rect.right - 10,
        rect.top + 10,
        rgb(78, 95, 103),
        3,
    );
}

unsafe fn paint_countdown(
    hdc: HDC,
    ui: Layout,
    remaining: u32,
    started: bool,
    total_seconds: u32,
) {
    let ring = ui.countdown;
    let size = ring.right - ring.left;

    draw_round(
        hdc,
        RECT {
            left: ring.left - 10,
            top: ring.top - 10,
            right: ring.right + 10,
            bottom: ring.bottom + 10,
        },
        rgb(255, 255, 255),
        rgb(226, 237, 236),
        1,
        size,
    );
    draw_ellipse(
        hdc,
        ring,
        rgb(255, 255, 255),
        rgb(221, 238, 232),
        12,
    );

    let progress = if started {
        remaining as f32 / total_seconds.max(1) as f32
    } else {
        1.0
    };
    draw_arc(hdc, ring, progress.clamp(0.0, 1.0), rgb(36, 148, 131), 12);

    draw_text(
        hdc,
        &remaining.to_string(),
        RECT {
            left: ring.left,
            top: ring.top + size * 26 / 100,
            right: ring.right,
            bottom: ring.top + size * 66 / 100,
        },
        (size * 38 / 100).max(48),
        FW_BOLD.0 as i32,
        rgb(31, 126, 102),
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );
    draw_text(
        hdc,
        "秒",
        RECT {
            left: ring.left,
            top: ring.top + size * 63 / 100,
            right: ring.right,
            bottom: ring.top + size * 84 / 100,
        },
        21,
        FW_MEDIUM.0 as i32,
        rgb(86, 123, 110),
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );
}

unsafe fn paint_tips(hdc: HDC, ui: Layout, kind: ReminderKind) {
    draw_round(hdc, ui.tip_left, rgb(251, 255, 253), rgb(214, 231, 225), 2, 28);
    draw_round(hdc, ui.tip_right, rgb(251, 255, 253), rgb(214, 231, 225), 2, 28);
    draw_text(
        hdc,
        if kind == ReminderKind::Micro {
            "⌁  远眺 20 秒"
        } else {
            "⌁  起身走动"
        },
        ui.tip_left,
        19,
        FW_MEDIUM.0 as i32,
        rgb(59, 78, 70),
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );
    draw_text(
        hdc,
        if kind == ReminderKind::Micro {
            "⌒  完整眨眼 5-10 次"
        } else {
            "⌒  补水 放松肩颈"
        },
        ui.tip_right,
        19,
        FW_MEDIUM.0 as i32,
        rgb(59, 78, 70),
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );
}

unsafe fn paint_buttons(hdc: HDC, layout: Layout, started: bool) {
    draw_round(
        hdc,
        layout.primary,
        rgb(47, 160, 132),
        rgb(47, 160, 132),
        1,
        28,
    );
    draw_text(
        hdc,
        if started { "完成休息" } else { "▶  开始休息" },
        layout.primary,
        23,
        FW_MEDIUM.0 as i32,
        rgb(255, 255, 255),
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );

    draw_round(
        hdc,
        layout.postpone,
        rgb(252, 255, 254),
        rgb(219, 230, 225),
        2,
        28,
    );
    draw_text(
        hdc,
        "延后 5 分钟",
        layout.postpone,
        20,
        FW_MEDIUM.0 as i32,
        rgb(58, 78, 70),
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );

    draw_round(
        hdc,
        layout.skip,
        rgb(252, 255, 254),
        rgb(219, 230, 225),
        2,
        28,
    );
    draw_text(
        hdc,
        "跳过",
        layout.skip,
        20,
        FW_MEDIUM.0 as i32,
        rgb(58, 78, 70),
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );
}

unsafe fn fill(hdc: HDC, rect: RECT, color: COLORREF) {
    let brush = CreateSolidBrush(color);
    FillRect(hdc, &rect, brush);
    let _ = DeleteObject(brush.into());
}

unsafe fn draw_round(
    hdc: HDC,
    rect: RECT,
    fill_color: COLORREF,
    stroke_color: COLORREF,
    stroke_width: i32,
    radius: i32,
) {
    let brush = CreateSolidBrush(fill_color);
    let pen = CreatePen(PS_SOLID, stroke_width, stroke_color);
    let old_brush = SelectObject(hdc, brush.into());
    let old_pen = SelectObject(hdc, pen.into());
    let _ = RoundRect(
        hdc,
        rect.left,
        rect.top,
        rect.right,
        rect.bottom,
        radius,
        radius,
    );
    restore_object(hdc, old_pen);
    restore_object(hdc, old_brush);
    let _ = DeleteObject(pen.into());
    let _ = DeleteObject(brush.into());
}

unsafe fn draw_ellipse(
    hdc: HDC,
    rect: RECT,
    fill_color: COLORREF,
    stroke_color: COLORREF,
    stroke_width: i32,
) {
    let brush = if fill_color.0 == 0 {
        HBRUSH(GetStockObject(NULL_BRUSH).0)
    } else {
        CreateSolidBrush(fill_color)
    };
    let pen = CreatePen(PS_SOLID, stroke_width, stroke_color);
    let old_brush = SelectObject(hdc, brush.into());
    let old_pen = SelectObject(hdc, pen.into());
    let _ = Ellipse(hdc, rect.left, rect.top, rect.right, rect.bottom);
    restore_object(hdc, old_pen);
    restore_object(hdc, old_brush);
    let _ = DeleteObject(pen.into());
    if fill_color.0 != 0 {
        let _ = DeleteObject(brush.into());
    }
}

unsafe fn draw_line(
    hdc: HDC,
    x1: i32,
    y1: i32,
    x2: i32,
    y2: i32,
    color: COLORREF,
    width: i32,
) {
    let pen = CreatePen(PS_SOLID, width, color);
    let old_pen = SelectObject(hdc, pen.into());
    let _ = MoveToEx(hdc, x1, y1, None);
    let _ = LineTo(hdc, x2, y2);
    restore_object(hdc, old_pen);
    let _ = DeleteObject(pen.into());
}

unsafe fn draw_polygon(hdc: HDC, points: &[(i32, i32)], color: COLORREF) {
    let pts: Vec<POINT> = points
        .iter()
        .map(|(x, y)| POINT { x: *x, y: *y })
        .collect();
    let brush = CreateSolidBrush(color);
    let pen = CreatePen(PS_SOLID, 1, color);
    let old_brush = SelectObject(hdc, brush.into());
    let old_pen = SelectObject(hdc, pen.into());
    let _ = Polygon(hdc, &pts);
    restore_object(hdc, old_pen);
    restore_object(hdc, old_brush);
    let _ = DeleteObject(pen.into());
    let _ = DeleteObject(brush.into());
}

unsafe fn draw_arc(hdc: HDC, rect: RECT, progress: f32, color: COLORREF, width: i32) {
    if progress <= 0.01 {
        return;
    }
    if progress >= 0.99 {
        draw_ellipse(hdc, rect, COLORREF(0), color, width);
        return;
    }
    let center_x = (rect.left + rect.right) as f32 / 2.0;
    let center_y = (rect.top + rect.bottom) as f32 / 2.0;
    let radius_x = (rect.right - rect.left) as f32 / 2.0;
    let radius_y = (rect.bottom - rect.top) as f32 / 2.0;
    let point = |degrees: f32| {
        let radians = degrees.to_radians();
        (
            (center_x + radius_x * radians.cos()).round() as i32,
            (center_y + radius_y * radians.sin()).round() as i32,
        )
    };
    let (start_x, start_y) = point(-90.0);
    let (end_x, end_y) = point(-90.0 + progress * 360.0);
    let pen = CreatePen(PS_SOLID, width, color);
    let old_pen = SelectObject(hdc, pen.into());
    let old_brush = SelectObject(hdc, HBRUSH(GetStockObject(NULL_BRUSH).0).into());
    let _ = Arc(
        hdc,
        rect.left,
        rect.top,
        rect.right,
        rect.bottom,
        start_x,
        start_y,
        end_x,
        end_y,
    );
    restore_object(hdc, old_brush);
    restore_object(hdc, old_pen);
    let _ = DeleteObject(pen.into());
}

unsafe fn draw_text(
    hdc: HDC,
    text: &str,
    mut rect: RECT,
    size: i32,
    weight: i32,
    color: COLORREF,
    flags: windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT,
) {
    let face = wide("Microsoft YaHei UI");
    let font = CreateFontW(
        -size,
        0,
        0,
        0,
        weight,
        0,
        0,
        0,
        DEFAULT_CHARSET,
        OUT_DEFAULT_PRECIS,
        CLIP_DEFAULT_PRECIS,
        DEFAULT_QUALITY,
        DEFAULT_PITCH.0 as u32,
        PCWSTR(face.as_ptr()),
    );
    let old_font = SelectObject(hdc, font.into());
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, color);
    let mut encoded = wide(text);
    let _ = DrawTextW(hdc, &mut encoded, &mut rect, flags);
    restore_object(hdc, old_font);
    let _ = DeleteObject(font.into());
}

unsafe fn restore_object(hdc: HDC, object: HGDIOBJ) {
    if !object.0.is_null() {
        let _ = SelectObject(hdc, object);
    }
}

fn rgb(red: u8, green: u8, blue: u8) -> COLORREF {
    COLORREF(red as u32 | ((green as u32) << 8) | ((blue as u32) << 16))
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
