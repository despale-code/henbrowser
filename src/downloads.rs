use std::sync::atomic::{AtomicU64, Ordering};
use tao::event_loop::EventLoopProxy;
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{
    BytesReceivedChangedEventHandler, DownloadStartingEventHandler, StateChangedEventHandler,
};
use windows::core::Interface;

use crate::{internal, UserEvent};

pub(crate) fn shell_execute(verb: &str, file: &str, params: Option<&str>) {
    use windows::core::HSTRING;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let params = params.map(HSTRING::from);
    unsafe {
        ShellExecuteW(
            None,
            &HSTRING::from(verb),
            &HSTRING::from(file),
            params
                .as_ref()
                .map_or(windows::core::PCWSTR::null(), |p| windows::core::PCWSTR(p.as_ptr())),
            None,
            SW_SHOWNORMAL,
        );
    }
}

/// Reads a download's current progress and state.
type Snapshot = std::rc::Rc<dyn Fn(&ICoreWebView2DownloadOperation) -> internal::Download>;

thread_local! {
    /// Live downloads by id: the operation and how to read it. Download events arrive on the UI
    /// thread, which is the only thread that touches this.
    static OPS: std::cell::RefCell<std::collections::HashMap<u64, (ICoreWebView2DownloadOperation, Snapshot)>> =
        Default::default();
}

/// Runs `f` on download `id`'s operation. The table is released first: Pause / Resume / Cancel
/// raise StateChanged synchronously, and its handler updates the table.
fn with_op(id: u64, f: impl FnOnce(&ICoreWebView2DownloadOperation)) {
    let op = OPS.with_borrow(|ops| ops.get(&id).map(|(op, _)| op.clone()));
    if let Some(op) = op {
        f(&op);
    }
}

/// A download that will not change any more (no need to keep watching it).
fn is_final(d: &internal::Download) -> bool {
    d.state == "completed" || d.state == "cancelled" || (d.state == "interrupted" && !d.can_resume)
}

/// Current snapshots of every live download, forgetting the finished ones. Polled as a backstop:
/// a missed StateChanged event would otherwise leave a finished download "in progress" forever.
pub(crate) fn poll() -> Vec<internal::Download> {
    let live: Vec<(ICoreWebView2DownloadOperation, Snapshot)> =
        OPS.with_borrow(|ops| ops.values().map(|(op, s)| (op.clone(), s.clone())).collect());
    let snaps: Vec<internal::Download> = live.iter().map(|(op, snapshot)| snapshot(op)).collect();
    OPS.with_borrow_mut(|ops| {
        for d in snaps.iter().filter(|d| is_final(d)) {
            ops.remove(&d.id);
        }
    });
    snaps
}

pub(crate) fn cancel(id: u64) {
    with_op(id, |op| {
        let _ = unsafe { op.Cancel() };
    });
}

/// Pauses a running download, or resumes a paused (or resumable interrupted) one.
pub(crate) fn toggle_pause(id: u64) {
    with_op(id, |op| unsafe {
        let mut state = COREWEBVIEW2_DOWNLOAD_STATE::default();
        let _ = op.State(&mut state);
        if state == COREWEBVIEW2_DOWNLOAD_STATE_IN_PROGRESS {
            let _ = op.Pause();
        } else {
            let _ = op.Resume();
        }
    });
}

pub(crate) fn set_clipboard_text(text: &str) {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    const CF_UNICODETEXT: u32 = 13;
    let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    unsafe {
        if OpenClipboard(None).is_err() {
            return;
        }
        let _ = EmptyClipboard();
        if let Ok(h) = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2) {
            let p = GlobalLock(h) as *mut u16;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
                let _ = GlobalUnlock(h);
                let _ = SetClipboardData(CF_UNICODETEXT, Some(HANDLE(h.0)));
            }
        }
        let _ = CloseClipboard();
    }
}

pub(crate) fn open_file(path: &str) {
    shell_execute("open", path, None);
}

pub(crate) fn show_in_folder(path: &str) {
    shell_execute("open", "explorer.exe", Some(&format!("/select,\"{path}\"")));
}

/// Tracks downloads for the shelf and `http://hen.downloads/`, in place of WebView2's own
/// download flyout.
pub(crate) fn install_downloads(
    controller: &ICoreWebView2Controller,
    proxy: EventLoopProxy<UserEvent>,
    download_dir: String,
) {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    let Ok(core4) =
        (unsafe { controller.CoreWebView2() }).and_then(|c| c.cast::<ICoreWebView2_4>())
    else {
        return;
    };
    let mut token = 0i64;
    unsafe {
        let _ = core4.add_DownloadStarting(
            &DownloadStartingEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else { return Ok(()) };
                let op = args.DownloadOperation()?;
                let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
                // The shelf shows it instead of the default flyout.
                let _ = args.SetHandled(true);
                let url = {
                    let mut p = windows::core::PWSTR::null();
                    op.Uri(&mut p)?;
                    webview2_com::take_pwstr(p)
                };
                let mut path = {
                    let mut p = windows::core::PWSTR::null();
                    args.ResultFilePath(&mut p)?;
                    webview2_com::take_pwstr(p)
                };
                if !download_dir.is_empty() {
                    let dir = std::path::Path::new(&download_dir);
                    if dir.exists() {
                        if let Some(fname) = std::path::Path::new(&path).file_name() {
                            let new_path = dir.join(fname);
                            let new_path_str = new_path.to_string_lossy().to_string();
                            let hstr = windows::core::HSTRING::from(&new_path_str);
                            let _ = args.SetResultFilePath(&hstr);
                            path = new_path_str;
                        }
                    }
                }
                // Snapshot of the operation's progress as a `Download`.
                let snapshot: Snapshot = std::rc::Rc::new({
                    let (url, path) = (url.clone(), path.clone());
                    move |op: &ICoreWebView2DownloadOperation| -> internal::Download {
                        let (mut received, mut total) = (0i64, 0i64);
                        let mut state = COREWEBVIEW2_DOWNLOAD_STATE::default();
                        let mut result_path = windows::core::PWSTR::null();
                        let _ = op.BytesReceived(&mut received);
                        let _ = op.TotalBytesToReceive(&mut total);
                        let _ = op.State(&mut state);
                        let _ = op.ResultFilePath(&mut result_path);
                        let p = webview2_com::take_pwstr(result_path);
                        let mut reason = COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON::default();
                        let _ = op.InterruptReason(&mut reason);
                        let mut can_resume = windows::core::BOOL(0);
                        let _ = op.CanResume(&mut can_resume);
                        internal::Download {
                            id,
                            url: url.clone(),
                            path: if p.is_empty() { path.clone() } else { p },
                            received: received.max(0) as u64,
                            total: total.max(0) as u64,
                            state: match state {
                                COREWEBVIEW2_DOWNLOAD_STATE_COMPLETED => "completed",
                                COREWEBVIEW2_DOWNLOAD_STATE_INTERRUPTED
                                    if reason == COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON_USER_PAUSED =>
                                {
                                    "paused"
                                }
                                COREWEBVIEW2_DOWNLOAD_STATE_INTERRUPTED
                                    if reason == COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON_USER_CANCELED =>
                                {
                                    "cancelled"
                                }
                                COREWEBVIEW2_DOWNLOAD_STATE_INTERRUPTED => "interrupted",
                                _ => "in_progress",
                            }
                            .to_owned(),
                            can_resume: can_resume.as_bool(),
                        }
                    }
                });
                OPS.with_borrow_mut(|ops| ops.insert(id, (op.clone(), snapshot.clone())));
                let _ = proxy.send_event(UserEvent::Download(snapshot(&op)));
                let mut t = 0i64;
                let (p1, s1) = (proxy.clone(), snapshot.clone());
                let _ = op.add_BytesReceivedChanged(
                    &BytesReceivedChangedEventHandler::create(Box::new(move |op, _| {
                        if let Some(op) = op {
                            let _ = p1.send_event(UserEvent::Download(s1(&op)));
                        }
                        Ok(())
                    })),
                    &mut t,
                );
                let (p2, s2) = (proxy.clone(), snapshot);
                let _ = op.add_StateChanged(
                    &StateChangedEventHandler::create(Box::new(move |op, _| {
                        if let Some(op) = op {
                            let d = s2(&op);
                            if is_final(&d) {
                                OPS.with_borrow_mut(|ops| ops.remove(&d.id));
                            }
                            let _ = p2.send_event(UserEvent::Download(d));
                        }
                        Ok(())
                    })),
                    &mut t,
                );
                Ok(())
            })),
            &mut token,
        );
    }
}
