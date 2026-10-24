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

fn resolve_downloader_exe() -> std::path::PathBuf {
    if let Ok(mut exe) = std::env::current_exe() {
        exe.set_file_name("chickenwm-downloader.exe");
        if exe.exists() {
            return exe;
        }
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let candidate = std::path::PathBuf::from(local)
            .join("Programs")
            .join("ChickenWM")
            .join("chickenwm-downloader.exe");
        if candidate.exists() {
            return candidate;
        }
    }
    std::path::PathBuf::from("chickenwm-downloader.exe")
}

/// Opens or toggles the `chickenwm-downloader` service panel.
pub(crate) fn open_downloader_window() {
    std::thread::spawn(|| {
        if chickenwm_core::ipc::send_ipc_command_blocking(
            chickenwm_core::ipc::PIPE_NAME_DOWNLOADER,
            &chickenwm_core::ipc::IpcCommand::ToggleDownloaderCenter,
        )
        .is_ok()
        {
            return;
        }
        let exe = resolve_downloader_exe();
        chickenwm_core::elevation::shell_open_unelevated(&exe, Some("--toggle"), None);
    });
}

/// Redirects a browser download or magnet/torrent request to `chickenwm-downloader`.
pub(crate) fn redirect_to_downloader(
    url: &str,
    filename: Option<&str>,
    save_dir: Option<&str>,
    is_torrent: bool,
) {
    redirect_to_downloader_with_context(url, filename, save_dir, is_torrent, None, None, None);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn redirect_to_downloader_with_context(
    url: &str,
    filename: Option<&str>,
    save_dir: Option<&str>,
    is_torrent: bool,
    cookies: Option<String>,
    referer: Option<String>,
    user_agent: Option<String>,
) {
    let url_owned = url.to_string();
    let fname_owned = filename.map(str::to_string);
    let dir_owned = save_dir.map(str::to_string);

    std::thread::spawn(move || {
        let cmd = chickenwm_core::ipc::IpcCommand::AddDownload {
            url: url_owned.clone(),
            filename: fname_owned.clone(),
            save_dir: dir_owned.clone(),
            is_torrent,
            prompt_location: true,
            cookies,
            referer,
            user_agent,
        };
        if chickenwm_core::ipc::send_ipc_command_blocking(
            chickenwm_core::ipc::PIPE_NAME_DOWNLOADER,
            &cmd,
        )
        .is_ok()
        {
            return;
        }

        // `chickenwm-downloader` service is not running yet; launch it with the download arguments.
        use std::os::windows::process::CommandExt;
        use std::process::Stdio;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        let exe = resolve_downloader_exe();
        let mut proc = std::process::Command::new(&exe);
        proc.arg("--add-url").arg(&url_owned).arg("--prompt-location");
        if let Some(ref f) = fname_owned {
            if !f.is_empty() {
                proc.arg("--filename").arg(f);
            }
        }
        if let Some(ref d) = dir_owned {
            if !d.is_empty() {
                proc.arg("--save-dir").arg(d);
            }
        }
        if is_torrent {
            proc.arg("--torrent");
        }
        proc.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW);
        let _ = proc.spawn();
    });
}

fn collect_cookie_list(list: Option<&ICoreWebView2CookieList>) -> String {
    let Some(list) = list else {
        return String::new();
    };
    let mut count = 0u32;
    if unsafe { list.Count(&mut count) }.is_err() || count == 0 {
        return String::new();
    }
    let mut pairs = Vec::with_capacity(count as usize);
    for i in 0..count {
        if let Ok(cookie) = unsafe { list.GetValueAtIndex(i) } {
            let mut n = windows::core::PWSTR::null();
            let mut v = windows::core::PWSTR::null();
            if unsafe { cookie.Name(&mut n) }.is_ok() && unsafe { cookie.Value(&mut v) }.is_ok() {
                let name = webview2_com::take_pwstr(n);
                let val = webview2_com::take_pwstr(v);
                if !name.is_empty() {
                    pairs.push(format!("{name}={val}"));
                }
            }
        }
    }
    pairs.join("; ")
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
    let core_for_cb = core4.clone();
    let mut token = 0i64;
    unsafe {
        let _ = core4.add_DownloadStarting(
            &DownloadStartingEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else { return Ok(()) };
                let op = args.DownloadOperation()?;
                let _ = args.SetHandled(true);
                let url = {
                    let mut p = windows::core::PWSTR::null();
                    op.Uri(&mut p)?;
                    webview2_com::take_pwstr(p)
                };
                let result_path = {
                    let mut p = windows::core::PWSTR::null();
                    args.ResultFilePath(&mut p)?;
                    webview2_com::take_pwstr(p)
                };
                let filename = std::path::Path::new(&result_path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(str::to_string);
                let u_lower = url.trim().to_ascii_lowercase();

                // `blob:` and `data:` URLs only exist inside the WebView2 process memory and cannot
                // be fetched by an external HTTP client; let WebView2 write them directly.
                if !u_lower.starts_with("blob:") && !u_lower.starts_with("data:") {
                    let is_torrent = u_lower.starts_with("magnet:")
                        || u_lower.split('?').next().unwrap_or("").ends_with(".torrent")
                        || filename
                            .as_deref()
                            .is_some_and(|f| f.to_ascii_lowercase().ends_with(".torrent"));

                    let referer = {
                        let mut p = windows::core::PWSTR::null();
                        if core_for_cb.Source(&mut p).is_ok() {
                            let s = webview2_com::take_pwstr(p);
                            (!s.is_empty() && s != "about:blank").then_some(s)
                        } else {
                            None
                        }
                    };
                    let ua = Some(crate::user_agent().to_string());
                    let save_dir_opt = (!download_dir.is_empty()).then(|| download_dir.clone());

                    // Cancel internal WebView2 download and redirect to `chickenwm-downloader`
                    // along with browser cookies for authenticated / Google Drive downloads.
                    let _ = op.Cancel();

                    if let Ok(mgr) = core_for_cb
                        .cast::<ICoreWebView2_2>()
                        .and_then(|c2| c2.CookieManager())
                    {
                        let url_cb = url.clone();
                        let fname_cb = filename.clone();
                        let dir_cb = save_dir_opt.clone();
                        let ref_cb = referer.clone();
                        let ua_cb = ua.clone();
                        let mgr_inner = mgr.clone();

                        let query_uri = windows::core::HSTRING::from(&url);
                        if mgr
                            .GetCookies(
                                &query_uri,
                                &webview2_com::GetCookiesCompletedHandler::create(Box::new(
                                    move |_, cookie_list| {
                                        let mut cookie_str = collect_cookie_list(cookie_list.as_ref());
                                        // For Google Drive redirects (`googleusercontent.com` / `drive.usercontent.google.com`),
                                        // also merge cookies from `https://drive.google.com`.
                                        let ul = url_cb.to_ascii_lowercase();
                                        if ul.contains("google") || ul.contains("gdrive") {
                                            let gdrive_uri = windows::core::HSTRING::from("https://drive.google.com");
                                            let url_cb2 = url_cb.clone();
                                            let fname_cb2 = fname_cb.clone();
                                            let dir_cb2 = dir_cb.clone();
                                            let ref_cb2 = ref_cb.clone();
                                            let ua_cb2 = ua_cb.clone();
                                            let mut cookie_str2 = cookie_str.clone();
                                            if mgr_inner
                                                .GetCookies(
                                                    &gdrive_uri,
                                                    &webview2_com::GetCookiesCompletedHandler::create(Box::new(
                                                        move |_, g_list| {
                                                            let g_cookies = collect_cookie_list(g_list.as_ref());
                                                            if !g_cookies.is_empty() {
                                                                if !cookie_str2.is_empty() {
                                                                    cookie_str2.push_str("; ");
                                                                }
                                                                cookie_str2.push_str(&g_cookies);
                                                            }
                                                            redirect_to_downloader_with_context(
                                                                &url_cb2,
                                                                fname_cb2.as_deref(),
                                                                dir_cb2.as_deref(),
                                                                is_torrent,
                                                                (!cookie_str2.is_empty()).then_some(cookie_str2),
                                                                ref_cb2,
                                                                ua_cb2,
                                                            );
                                                            Ok(())
                                                        },
                                                    )),
                                                )
                                                .is_ok()
                                            {
                                                return Ok(());
                                            }
                                        }
                                        redirect_to_downloader_with_context(
                                            &url_cb,
                                            fname_cb.as_deref(),
                                            dir_cb.as_deref(),
                                            is_torrent,
                                            (!cookie_str.is_empty()).then_some(cookie_str),
                                            ref_cb,
                                            ua_cb,
                                        );
                                        Ok(())
                                    },
                                )),
                            )
                            .is_ok()
                        {
                            return Ok(());
                        }
                    }

                    redirect_to_downloader_with_context(
                        &url,
                        filename.as_deref(),
                        save_dir_opt.as_deref(),
                        is_torrent,
                        None,
                        referer,
                        ua,
                    );
                    return Ok(());
                }
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
