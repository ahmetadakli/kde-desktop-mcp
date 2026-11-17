//! Starting apps: resolve a name through App Paths, PATH, the Windows
//! folders or a Store app's manifest (never a shell), start it detached,
//! and list processes to follow the started one's children.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_SUCCESS, HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::System::Com::{
    CLSCTX_LOCAL_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, RRF_RT_REG_SZ, RegCloseKey,
    RegEnumKeyExW, RegGetValueW, RegOpenKeyExW,
};
use windows::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_UNICODE_ENVIRONMENT,
    CreateProcessW, DETACHED_PROCESS, GetExitCodeProcess, INFINITE, PROCESS_INFORMATION,
    STARTUPINFOW, WaitForSingleObject,
};
use windows::Win32::UI::Shell::{
    AO_NONE, ApplicationActivationManager, IApplicationActivationManager,
};
use windows::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow};
use windows::core::{HSTRING, PCWSTR, PWSTR};

use super::logic;
use crate::backend::R;
use crate::launch_pick::command_line;

/// What `launch` starts.
pub enum Target {
    /// A program file, started with CreateProcess.
    Program(PathBuf),
    /// A Store app by its AppUserModelID, started through the shell.
    Store(String),
}

pub struct Resolved {
    pub target: Target,
    pub args: Vec<String>,
}

/// The command as the reply shows it.
pub fn describe(r: &Resolved) -> String {
    match &r.target {
        Target::Program(p) => {
            let argv: Vec<String> = std::iter::once(p.to_string_lossy().into_owned())
                .chain(r.args.iter().cloned())
                .collect();
            command_line(&argv)
        }
        Target::Store(aumid) => {
            let mut s = format!("shell:AppsFolder\\{aumid}");
            if !r.args.is_empty() {
                s.push(' ');
                s.push_str(&command_line(&r.args));
            }
            s
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// A string value (`None`: the key's default value); REG_EXPAND_SZ values
/// come expanded.
fn reg_string(root: HKEY, key: &str, value: Option<&str>) -> Option<String> {
    let key = wide(key);
    let value = value.map(wide);
    let name = value
        .as_ref()
        .map_or(PCWSTR::null(), |v| PCWSTR(v.as_ptr()));
    let mut size = 0u32;
    let first = unsafe {
        RegGetValueW(
            root,
            PCWSTR(key.as_ptr()),
            name,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        )
    };
    if first != ERROR_SUCCESS || size == 0 {
        return None;
    }
    let mut buf = vec![0u16; (size as usize).div_ceil(2) + 1];
    let mut size = (buf.len() * 2) as u32;
    let ok = unsafe {
        RegGetValueW(
            root,
            PCWSTR(key.as_ptr()),
            name,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if ok != ERROR_SUCCESS {
        return None;
    }
    let n = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..n]))
}

/// The names of a key's subkeys.
fn reg_subkeys(root: HKEY, key: &str) -> Vec<String> {
    let key = wide(key);
    let mut h = HKEY::default();
    if unsafe { RegOpenKeyExW(root, PCWSTR(key.as_ptr()), None, KEY_READ, &mut h) } != ERROR_SUCCESS
    {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut buf = [0u16; 256];
    for i in 0.. {
        let mut len = buf.len() as u32;
        let r = unsafe {
            RegEnumKeyExW(
                h,
                i,
                Some(PWSTR(buf.as_mut_ptr())),
                &mut len,
                None,
                None,
                None,
                None,
            )
        };
        if r != ERROR_SUCCESS {
            break;
        }
        out.push(String::from_utf16_lossy(&buf[..len as usize]));
    }
    unsafe {
        let _ = RegCloseKey(h);
    }
    out
}

const PACKAGES: &str = r"Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppModel\Repository\Packages";

/// The AppUserModelID of the user's Store app best matching `app` (see
/// `logic::store_rank`), from the installed packages' manifests.
fn store_app(app: &str) -> Option<String> {
    let mut best: Option<(u8, String)> = None;
    for full in reg_subkeys(HKEY_CURRENT_USER, PACKAGES) {
        let Some(family) = logic::package_family(&full) else {
            continue;
        };
        let key = format!("{PACKAGES}\\{full}");
        let Some(root) = reg_string(HKEY_CURRENT_USER, &key, Some("PackageRootFolder")) else {
            continue;
        };
        let Ok(xml) = std::fs::read_to_string(Path::new(&root).join("AppxManifest.xml")) else {
            continue;
        };
        let (name, apps) = logic::manifest_apps(&xml);
        for (id, exe) in apps {
            let Some(rank) = logic::store_rank(app, &name, &exe) else {
                continue;
            };
            if best.as_ref().is_none_or(|b| rank < b.0) {
                best = Some((rank, format!("{family}!{id}")));
            }
        }
        if best.as_ref().is_some_and(|b| b.0 == 0) {
            break;
        }
    }
    best.map(|b| b.1)
}

/// A file, or a Store execution alias (a 0-byte reparse point that only
/// CreateProcess follows).
fn exists(p: &Path) -> bool {
    p.symlink_metadata().is_ok_and(|m| !m.is_dir())
}

/// Find what to start for `app`: an App Paths entry, a program on PATH, in
/// System32 or the Windows folder, else a Store app.
pub fn resolve(app: &str, args: &[String]) -> R<Resolved> {
    let file = logic::exe_file(app);
    let env = |k: &str| std::env::var(k).ok();
    let key = format!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\{file}");
    let app_path = [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE]
        .into_iter()
        .find_map(|root| reg_string(root, &key, None))
        .and_then(|v| logic::app_path_value(&v, env));
    let path: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let dirs = logic::program_dirs(&path, std::env::var("SystemRoot").ok().as_deref());
    let target = match logic::find_program(&file, app_path, &dirs, exists) {
        Some(p) => Target::Program(p),
        None => match store_app(app) {
            Some(aumid) => Target::Store(aumid),
            None => {
                return Err(format!(
                    "no App Paths entry, program on PATH, System32 program or Store app named \
                     '{app}'"
                ));
            }
        },
    };
    Ok(Resolved {
        target,
        args: args.to_vec(),
    })
}

/// Exit code once the process has ended; a thread waits for it.
pub type Exit = Arc<Mutex<Option<u32>>>;

/// Start a program with no console, outside the server's job when allowed
/// (a client's kill-on-close job would end it with the server), in the
/// user's profile folder. It inherits no handles: holding the server's
/// stdio pipes would keep its client waiting for them to close after the
/// server ended.
pub fn spawn(program: &Path, args: &[String]) -> R<(u32, Exit)> {
    let mut line = format!("\"{}\"", program.display());
    if !args.is_empty() {
        line.push(' ');
        line.push_str(&logic::command_args(args));
    }
    let mut line: Vec<u16> = wide(&line);
    let dir = std::env::var("USERPROFILE").ok().map(|d| wide(&d));
    let dir = dir.as_ref().map_or(PCWSTR::null(), |d| PCWSTR(d.as_ptr()));
    let si = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_UNICODE_ENVIRONMENT;
    let mut pi = PROCESS_INFORMATION::default();
    let mut start = |flags| unsafe {
        CreateProcessW(
            PCWSTR::null(),
            Some(PWSTR(line.as_mut_ptr())),
            None,
            None,
            false,
            flags,
            None,
            dir,
            &si,
            &mut pi,
        )
    };
    unsafe {
        // A newly started app may take the foreground.
        let _ = AllowSetForegroundWindow(ASFW_ANY);
    }
    let r = match start(flags | CREATE_BREAKAWAY_FROM_JOB) {
        // The job does not allow breaking away.
        Err(e) if e.code() == ERROR_ACCESS_DENIED.to_hresult() => start(flags),
        r => r,
    };
    r.map_err(|e| format!("could not start {}: {}", program.display(), e.message()))?;
    unsafe {
        let _ = CloseHandle(pi.hThread);
    }
    let exit: Exit = Arc::new(Mutex::new(None));
    let done = exit.clone();
    // The handle as a number: a raw handle cannot cross threads.
    let process = pi.hProcess.0 as isize;
    std::thread::spawn(move || {
        let h = HANDLE(process as *mut _);
        let mut code = 0u32;
        unsafe {
            if WaitForSingleObject(h, INFINITE) == WAIT_OBJECT_0
                && GetExitCodeProcess(h, &mut code).is_ok()
            {
                *done.lock().unwrap() = Some(code);
            }
            let _ = CloseHandle(h);
        }
    });
    Ok((pi.dwProcessId, exit))
}

/// Start a Store app (blocking; the shell may take a few seconds) and
/// return its process id, which is a running instance's when it took the
/// request.
pub fn activate(aumid: &str, args: &[String]) -> R<u32> {
    unsafe {
        let _ = AllowSetForegroundWindow(ASFW_ANY);
        let init = CoInitializeEx(None, COINIT_MULTITHREADED);
        let r = (|| {
            let m: IApplicationActivationManager =
                CoCreateInstance(&ApplicationActivationManager, None, CLSCTX_LOCAL_SERVER)
                    .map_err(|e| {
                        format!("the shell's app activation is unavailable: {}", e.message())
                    })?;
            m.ActivateApplication(
                &HSTRING::from(aumid),
                &HSTRING::from(logic::command_args(args)),
                AO_NONE,
            )
            .map_err(|e| format!("could not start {aumid}: {}", e.message()))
        })();
        if init.is_ok() {
            CoUninitialize();
        }
        r
    }
}

/// (process id, parent process id) of every process.
pub fn parents() -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return out;
        };
        let mut e = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut more = Process32FirstW(snap, &mut e).is_ok();
        while more {
            out.push((e.th32ProcessID, e.th32ParentProcessID));
            more = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    out
}
