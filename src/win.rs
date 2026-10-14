//! Windows 底层调用：卷句柄、设备控制、盘符枚举。
//!
//! 只声明必要的 kernel32 函数，避免引入庞大的 FFI 依赖；
//! 打开卷句柄直接复用标准库的 OpenOptions（其默认共享模式恰好满足需求）。

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;

/// FSCTL_ENUM_USN_DATA —— 枚举卷上所有 USN 记录（等价于遍历整张 MFT）
pub const FSCTL_ENUM_USN_DATA: u32 = 0x0009_00B3;

/// FILETIME(1601-01-01 起 100ns) → unix 秒 的偏移
pub const FILETIME_UNIX_DIFF: i64 = 11_644_473_600;

const DRIVE_FIXED: u32 = 3;
const FILE_SHARE_READ: u32 = 0x0000_0001;
const FILE_SHARE_WRITE: u32 = 0x0000_0002;
const FILE_SHARE_DELETE: u32 = 0x0000_0004;

// 标准流句柄槽位（Win32 里是负数的 DWORD）
const STD_INPUT_HANDLE: u32 = -10i32 as u32;
const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
const STD_ERROR_HANDLE: u32 = -12i32 as u32;

const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
const INVALID_HANDLE_VALUE: isize = -1;
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const OPEN_EXISTING: u32 = 3;

#[link(name = "kernel32")]
extern "system" {
    fn DeviceIoControl(
        handle: isize,
        io_control_code: u32,
        in_buffer: *const u8,
        in_size: u32,
        out_buffer: *mut u8,
        out_size: u32,
        bytes_returned: *mut u32,
        overlapped: *mut u8,
    ) -> i32;

    fn GetLogicalDrives() -> u32;
    fn GetDriveTypeW(root_path_name: *const u16) -> u32;
    fn GetVolumeInformationW(
        root_path_name: *const u16,
        volume_name: *mut u16,
        volume_name_size: u32,
        serial: *mut u32,
        max_component_len: *mut u32,
        flags: *mut u32,
        fs_name: *mut u16,
        fs_name_size: u32,
    ) -> i32;

    fn GetStdHandle(std_handle: u32) -> isize;
    fn SetStdHandle(std_handle: u32, handle: isize) -> i32;
    fn AttachConsole(process_id: u32) -> i32;
    fn CreateFileW(
        file_name: *const u16,
        desired_access: u32,
        share_mode: u32,
        security_attributes: *mut u8,
        creation_disposition: u32,
        flags_and_attributes: u32,
        template_file: isize,
    ) -> isize;
}

#[link(name = "shell32")]
extern "system" {
    fn ShellExecuteW(
        hwnd: isize,
        operation: *const u16,
        file: *const u16,
        parameters: *const u16,
        directory: *const u16,
        show_cmd: i32,
    ) -> isize;
}

/// 启动时的标准流处理方式。
pub enum ConsoleMode {
    /// CLI 子命令：允许接回调用者的控制台，好让 `mftfind search` 照常往终端打印。
    Cli,
    /// 图形界面：绝不接控制台。接了的话，用户一关终端就会连同界面一起被带走。
    /// 这里只保证标准句柄不会「无效到让 println! panic」。
    Gui,
}

/// 修正标准流句柄。
///
/// 背景：release 版本以 `IMAGE_SUBSYSTEM_WINDOWS` 链接（见 main.rs 顶部的
/// `windows_subsystem`），好处是从资源管理器双击不再弹出控制台窗口，
/// 代价是进程被创建时**不再自动获得标准句柄**——此时
/// `GetStdHandle` 返回空，任何 `println!` / `eprintln!` 都会写失败并 panic。
///
/// 所以这里逐一检查三个槽位：
///   * 句柄已经有效 → 原样保留。重定向（`> out.txt`）、管道、IDE 传入的句柄
///     都属于这一类，替换掉就会破坏这些用法。
///   * 句柄无效 → CLI 模式下尝试接回父控制台并用 `CONOUT$` / `CONIN$`，
///     接不上（双击启动、父进程没有控制台）以及 GUI 模式一律指向 `NUL`，
///     让输出被安静丢弃而不是让程序崩溃。
pub fn init_console(mode: ConsoleMode) {
    let attached = match mode {
        // 没有父控制台时返回 0，且**不会新建窗口**，所以双击启动也不会闪一下。
        ConsoleMode::Cli => unsafe { AttachConsole(ATTACH_PARENT_PROCESS) != 0 },
        ConsoleMode::Gui => false,
    };

    for (slot, console_device) in [
        (STD_INPUT_HANDLE, "CONIN$"),
        (STD_OUTPUT_HANDLE, "CONOUT$"),
        (STD_ERROR_HANDLE, "CONOUT$"),
    ] {
        if handle_valid(slot) {
            continue;
        }
        let device = if attached { console_device } else { "NUL" };
        let access = if slot == STD_INPUT_HANDLE {
            GENERIC_READ
        } else {
            GENERIC_WRITE
        };
        let handle = unsafe {
            CreateFileW(
                wide(device).as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                0,
                0,
            )
        };
        if handle != INVALID_HANDLE_VALUE && handle != 0 {
            unsafe { SetStdHandle(slot, handle) };
        }
    }
}

fn handle_valid(slot: u32) -> bool {
    let h = unsafe { GetStdHandle(slot) };
    h != 0 && h != INVALID_HANDLE_VALUE
}

/// 用系统默认程序打开文件；目录则打开资源管理器
pub fn open_path(path: &str, is_dir: bool) -> std::io::Result<()> {
    let op = wide("open");
    let file = wide(path);
    let r = unsafe {
        ShellExecuteW(
            0,
            op.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            if is_dir { 1 } else { 1 },
        )
    };
    // ShellExecuteW 返回值 <= 32 表示失败
    if r <= 32 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// 打开文件所在目录并选中该文件
pub fn reveal_in_explorer(path: &str) -> std::io::Result<()> {
    let op = wide("open");
    let exe = wide("explorer.exe");
    let params = wide(&format!("/select,\"{}\"", path));
    let r = unsafe {
        ShellExecuteW(
            0,
            op.as_ptr(),
            exe.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            1,
        )
    };
    if r <= 32 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// 以管理员身份重启自身（弹出 UAC），成功后返回 true
pub fn restart_as_admin(extra_args: &str) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let op = wide("runas");
    let file = wide(&exe.to_string_lossy());
    let params = wide(extra_args);
    let r = unsafe {
        ShellExecuteW(
            0,
            op.as_ptr(),
            file.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            1,
        )
    };
    if r <= 32 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// 当前进程是否以管理员权限运行
pub fn is_elevated() -> bool {
    // 尝试打开卷：管理员可行，普通用户会被拒绝
    match Volume::open('C') {
        Ok(_) => true,
        Err(e) => !is_error_access_denied(&e),
    }
}

/// 卷句柄。持有 `\\.\X:` 的只读句柄，用于发送 FSCTL。
pub struct Volume {
    file: File,
    pub letter: char,
}

impl Volume {
    /// 打开卷（读取 MFT 需要管理员权限；权限不足会返回 ACCESS_DENIED）
    pub fn open(letter: char) -> io::Result<Volume> {
        let path = format!("\\\\.\\{}:", letter);
        let file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(path)?;
        Ok(Volume { file, letter })
    }

    /// 同步发送设备控制码
    pub fn ioctl(&self, code: u32, input: &[u8], output: &mut [u8]) -> io::Result<usize> {
        let mut returned: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                self.file.as_raw_handle() as isize,
                code,
                input.as_ptr(),
                input.len() as u32,
                output.as_mut_ptr(),
                output.len() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(returned as usize)
    }
}

/// 是否为管理员权限下的可用卷（用于决定是否走 MFT 快路径）
pub fn is_error_access_denied(e: &io::Error) -> bool {
    // ERROR_ACCESS_DENIED = 5
    e.raw_os_error() == Some(5)
}

/// ERROR_HANDLE_EOF = 38。
///
/// 注意：FSCTL_ENUM_USN_DATA 枚举到末尾时不是返回「0 字节成功」，
/// 而是**失败并置 ERROR_HANDLE_EOF**——这是文档规定的正常结束方式，
/// 必须当正常出口处理，否则整卷已解析的记录会被整体丢弃。
pub fn is_error_eof(e: &io::Error) -> bool {
    e.raw_os_error() == Some(38)
}

fn wide(path: &str) -> Vec<u16> {
    path.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 枚举本机固定磁盘盘符（跳过可移动盘与光驱）
pub fn fixed_drives() -> Vec<char> {
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let probe = wide(&format!("{}:\\", letter));
        let dtype = unsafe { GetDriveTypeW(probe.as_ptr()) };
        if dtype == DRIVE_FIXED {
            out.push(letter);
        }
    }
    out
}

/// 读取卷标与文件系统名（用于界面展示）
pub fn volume_info(letter: char) -> (String, String) {
    let root = wide(&format!("{}:\\", letter));
    let mut name = vec![0u16; 261];
    let mut fs = vec![0u16; 261];
    let mut serial = 0u32;
    let mut max_len = 0u32;
    let mut flags = 0u32;
    let ok = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            name.as_mut_ptr(),
            261,
            &mut serial,
            &mut max_len,
            &mut flags,
            fs.as_mut_ptr(),
            261,
        )
    };
    if ok == 0 {
        return (String::new(), String::new());
    }
    let trim = |v: Vec<u16>| {
        let end = v.iter().position(|&c| c == 0).unwrap_or(v.len());
        String::from_utf16_lossy(&v[..end])
    };
    (trim(name), trim(fs))
}
