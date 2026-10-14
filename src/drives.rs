//! Drives, elevation and Explorer integration.

use std::os::windows::process::CommandExt;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LUID};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, GetTokenInformation, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW,
    SE_BACKUP_NAME, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_ELEVATION, TOKEN_PRIVILEGES,
    TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW, GetVolumePathNameW,
    QueryDosDeviceW,
};
use windows_sys::Win32::System::Diagnostics::Debug::{SEM_FAILCRITICALERRORS, SetThreadErrorMode};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::model::{DriveInfo, DriveKind};

const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;
const DRIVE_REMOTE: u32 = 4;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Suppresses "insert a disk" error boxes for the calling thread while alive.
struct QuietErrors(u32);
impl QuietErrors {
    fn new() -> Self {
        let mut old = 0u32;
        unsafe { SetThreadErrorMode(SEM_FAILCRITICALERRORS, &mut old) };
        QuietErrors(old)
    }
}
impl Drop for QuietErrors {
    fn drop(&mut self) {
        unsafe { SetThreadErrorMode(self.0, std::ptr::null_mut()) };
    }
}

/// All drive letters with a volume, A..Z. Each drive is queried on its own thread; a drive that
/// does not answer within 1.5 s (a sleeping network share) is listed with no size.
pub fn list() -> Vec<DriveInfo> {
    let mask = unsafe { GetLogicalDrives() };
    let letters: Vec<char> = (0..26u8).filter(|i| mask & (1 << i) != 0).map(|i| (b'A' + i) as char).collect();
    let (tx, rx) = mpsc::channel();
    for (i, &l) in letters.iter().enumerate() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send((i, query(l, true)));
        });
    }
    drop(tx);
    let deadline = Instant::now() + Duration::from_millis(1500);
    let mut out: Vec<Option<Option<DriveInfo>>> = vec![None; letters.len()];
    while out.iter().any(Option::is_none) {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok((i, d)) => out[i] = Some(d),
            Err(_) => break,
        }
    }
    out.into_iter()
        .zip(&letters)
        .filter_map(|(d, &l)| match d {
            Some(d) => d,
            None => query(l, false), // timed out: type only
        })
        .collect()
}

fn query(letter: char, full: bool) -> Option<DriveInfo> {
    let root = format!("{letter}:\\");
    let w = wide(&root);
    let kind = match unsafe { GetDriveTypeW(w.as_ptr()) } {
        0 | 1 => return None, // unknown / no root directory
        DRIVE_FIXED => DriveKind::Fixed,
        DRIVE_REMOVABLE => DriveKind::Removable,
        DRIVE_REMOTE => DriveKind::Network,
        _ => DriveKind::Other,
    };
    let mut d = DriveInfo { root, letter, label: String::new(), total: 0, free: 0, kind };
    if !full {
        return Some(d);
    }
    let _quiet = QuietErrors::new();
    let mut label = [0u16; 261];
    let ok = unsafe {
        GetVolumeInformationW(
            w.as_ptr(),
            label.as_mut_ptr(),
            label.len() as u32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    if ok != 0 {
        d.label = from_wide(&label);
    }
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    if unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, &mut total, &mut free) } != 0 {
        d.total = total;
        d.free = free;
    } else if matches!(kind, DriveKind::Removable | DriveKind::Other) {
        return None; // empty card reader / optical drive without a disc
    }
    Some(d)
}

/// File system name of a volume root (`NTFS`, `exFAT`, `ReFS` …).
pub fn filesystem_name(root: &str) -> Option<String> {
    let w = wide(root);
    let mut fs = [0u16; 64];
    let _quiet = QuietErrors::new();
    let ok = unsafe {
        GetVolumeInformationW(
            w.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fs.as_mut_ptr(),
            fs.len() as u32,
        )
    };
    (ok != 0).then(|| from_wide(&fs))
}

/// True for a drive letter that is a local volume (fixed or removable disk), not a mapped
/// network share and not a SUBST alias of a folder. `root` is `C:\`.
pub fn is_local_volume(root: &str) -> bool {
    let w = wide(root);
    if !matches!(unsafe { GetDriveTypeW(w.as_ptr()) }, DRIVE_FIXED | DRIVE_REMOVABLE) {
        return false;
    }
    // a volume maps to \Device\HarddiskVolumeN; a SUBST drive to \??\C:\some\folder
    let Some(letter) = root.get(..2) else { return false };
    let name = wide(letter);
    let mut target = [0u16; 1024];
    let n = unsafe { QueryDosDeviceW(name.as_ptr(), target.as_mut_ptr(), target.len() as u32) };
    n != 0 && from_wide(&target).starts_with(r"\Device\")
}

/// True when the volume holding `path` supports hard links (NTFS, ReFS), i.e. one file can
/// show up under several names; false for FAT / exFAT or when unknown.
pub fn supports_hard_links(path: &str) -> bool {
    const FILE_SUPPORTS_HARD_LINKS: u32 = 0x0040_0000;
    let mut p = path.to_string();
    if !p.ends_with('\\') {
        p.push('\\');
    }
    let w = wide(&p);
    let mut root = [0u16; 1024];
    let _quiet = QuietErrors::new();
    if unsafe { GetVolumePathNameW(w.as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
        return false;
    }
    let mut flags = 0u32;
    let ok = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut flags,
            std::ptr::null_mut(),
            0,
        )
    };
    ok != 0 && flags & FILE_SUPPORTS_HARD_LINKS != 0
}

fn process_token(access: u32) -> Option<HANDLE> {
    let mut tok: HANDLE = std::ptr::null_mut();
    let ok = unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut tok) };
    (ok != 0).then_some(tok)
}

/// True when the process runs with an elevated (admin) token.
pub fn is_elevated() -> bool {
    let Some(tok) = process_token(TOKEN_QUERY) else { return false };
    let mut el = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut len = 0u32;
    let ok = unsafe {
        GetTokenInformation(
            tok,
            TokenElevation,
            (&mut el as *mut TOKEN_ELEVATION).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
    };
    unsafe { CloseHandle(tok) };
    ok != 0 && el.TokenIsElevated != 0
}

/// When elevated, enables SeBackupPrivilege so the folder walk can list folders whose ACLs
/// would otherwise deny it (opens use FILE_OPEN_FOR_BACKUP_INTENT). Read-only use. Once.
pub(crate) fn enable_backup_privilege() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if !is_elevated() {
            return;
        }
        let Some(tok) = process_token(TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY) else { return };
        let mut luid = LUID { LowPart: 0, HighPart: 0 };
        unsafe {
            if LookupPrivilegeValueW(std::ptr::null(), SE_BACKUP_NAME, &mut luid) != 0 {
                let tp = TOKEN_PRIVILEGES {
                    PrivilegeCount: 1,
                    Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
                };
                AdjustTokenPrivileges(tok, 0, &tp, 0, std::ptr::null_mut(), std::ptr::null_mut());
            }
            CloseHandle(tok);
        }
    });
}

/// Quotes one argument the way `CommandLineToArgvW` reads it back.
pub(crate) fn quote_arg(a: &str) -> String {
    if !a.is_empty() && !a.contains([' ', '\t', '\n', '\x0B', '"']) {
        return a.to_string();
    }
    let mut s = String::from("\"");
    let mut slashes = 0usize;
    for c in a.chars() {
        match c {
            '\\' => slashes += 1,
            '"' => {
                s.push_str(&"\\".repeat(slashes * 2 + 1));
                s.push('"');
                slashes = 0;
            }
            _ => {
                s.push_str(&"\\".repeat(slashes));
                s.push(c);
                slashes = 0;
            }
        }
    }
    s.push_str(&"\\".repeat(slashes * 2));
    s.push('"');
    s
}

/// Relaunches this exe with the same args through the UAC prompt ("runas"). The caller exits
/// afterwards. See `try_restart_elevated` to know whether the user accepted.
#[allow(dead_code)]
pub fn restart_elevated(args: &[String]) {
    let _ = try_restart_elevated(args);
}

/// Like `restart_elevated`, but returns false if the elevated instance was not started
/// (the user declined the UAC prompt), so the caller can stay open.
pub fn try_restart_elevated(args: &[String]) -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let params = args.iter().map(|a| quote_arg(a)).collect::<Vec<_>>().join(" ");
    let exe = wide(&exe.to_string_lossy());
    let params = wide(&params);
    let verb = wide("runas");
    let cwd = std::env::current_dir().ok().map(|d| wide(&d.to_string_lossy()));
    let r = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            exe.as_ptr(),
            params.as_ptr(),
            cwd.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            SW_SHOWNORMAL,
        )
    };
    r as isize > 32
}

/// Explorer's own argument syntax: quotes only when needed, no trailing backslash inside
/// quotes (except a drive root, which needs none).
fn explorer_path(path: &str) -> String {
    let p = if path.len() > 3 { path.trim_end_matches('\\') } else { path };
    if p.contains([' ', ',', ';', '=']) { format!("\"{p}\"") } else { p.to_string() }
}

fn explorer(arg: String) {
    let _ = std::process::Command::new("explorer.exe").raw_arg(arg).spawn();
}

/// Opens the folder in Explorer. For a file, opens its folder with the file selected.
pub fn open_in_explorer(path: &str) {
    if std::path::Path::new(path).is_dir() {
        explorer(explorer_path(path));
    } else {
        reveal_in_explorer(path);
    }
}

/// Opens the parent folder in Explorer with the item selected.
pub fn reveal_in_explorer(path: &str) {
    explorer(format!("/select,{}", explorer_path(path)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(quote_arg("C:\\x"), "C:\\x");
        assert_eq!(quote_arg("C:\\Program Files\\"), "\"C:\\Program Files\\\\\"");
        assert_eq!(quote_arg("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote_arg(""), "\"\"");
        assert_eq!(explorer_path("C:\\"), "C:\\");
        assert_eq!(explorer_path("C:\\Program Files\\"), "\"C:\\Program Files\"");
    }

    #[test]
    fn lists_drives() {
        let d = list();
        let c = d.iter().find(|d| d.letter == 'C').expect("C: drive");
        assert_eq!(c.kind, DriveKind::Fixed);
        assert!(c.total > c.free && c.total > 0);
        assert_eq!(filesystem_name("C:\\").as_deref(), Some("NTFS"));
        assert!(is_local_volume("C:\\"));
        assert!(supports_hard_links("C:\\"));
        assert!(supports_hard_links("C:\\Windows"));
    }
}
