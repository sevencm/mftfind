//! 免管理员的回退方案：并行目录遍历。
//!
//! 关键优化：不借助标准库的 read_dir + metadata（那会对每个文件多发起一次系统调用），
//! 而是直接调 FindFirstFileExW —— 一次调用就带回名字、属性、大小、时间戳，
//! 并开启 FIND_FIRST_EX_LARGE_FETCH 让内核批量返回目录数据。
//!
//! 与 MFT 路径相比，这里的唯一好处是不需要管理员权限，且能顺带拿到文件大小。

use std::path::PathBuf;

use rayon::prelude::*;

use crate::format::Entry;
use crate::win::FILETIME_UNIX_DIFF;

const ATTR_DIRECTORY: u32 = 0x0000_0010;
const ATTR_REPARSE_POINT: u32 = 0x0000_0400;

const FIND_EX_INFO_BASIC: u32 = 1;
const FIND_EX_SEARCH_NAME_MATCH: u32 = 0;
const FIND_FIRST_EX_LARGE_FETCH: u32 = 2;
const INVALID_HANDLE_VALUE: isize = -1;

#[repr(C)]
#[derive(Clone, Copy)]
struct Win32FindDataW {
    attributes: u32,
    creation_time: [u32; 2],
    access_time: [u32; 2],
    write_time: [u32; 2],
    size_high: u32,
    size_low: u32,
    reserved0: u32,
    reserved1: u32,
    file_name: [u16; 260],
    alt_file_name: [u16; 14],
}

#[link(name = "kernel32")]
extern "system" {
    fn FindFirstFileExW(
        file_name: *const u16,
        info_level: u32,
        find_data: *mut Win32FindDataW,
        search_op: u32,
        search_filter: *mut u8,
        flags: u32,
    ) -> isize;
    fn FindNextFileW(find: isize, find_data: *mut Win32FindDataW) -> i32;
    fn FindClose(find: isize) -> i32;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn filetime_to_unix(ft: [u32; 2]) -> i64 {
    let v = ((ft[1] as u64) << 32 | ft[0] as u64) as i64;
    if v == 0 {
        return 0;
    }
    v / 10_000_000 - FILETIME_UNIX_DIFF
}

struct Scanned {
    name: String,
    path: PathBuf,
    size: u64,
    mtime: i64,
    is_dir: bool,
    is_reparse: bool,
}

/// 列出一个目录的全部条目（单次系统调用批量取回）
fn scan_dir(path: &str) -> Vec<Scanned> {
    let pattern = wide(&format!("{}\\*", path.trim_end_matches('\\')));
    let mut data = Win32FindDataW {
        attributes: 0,
        creation_time: [0; 2],
        access_time: [0; 2],
        write_time: [0; 2],
        size_high: 0,
        size_low: 0,
        reserved0: 0,
        reserved1: 0,
        file_name: [0; 260],
        alt_file_name: [0; 14],
    };
    let mut out = Vec::new();
    let handle = unsafe {
        FindFirstFileExW(
            pattern.as_ptr(),
            FIND_EX_INFO_BASIC,
            &mut data,
            FIND_EX_SEARCH_NAME_MATCH,
            std::ptr::null_mut(),
            FIND_FIRST_EX_LARGE_FETCH,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return out;
    }

    loop {
        let end = data.file_name.iter().position(|&c| c == 0).unwrap_or(260);
        // 跳过 . 与 ..
        if end > 0 && !(end == 1 && data.file_name[0] == b'.' as u16) && !(end == 2 && data.file_name[0] == b'.' as u16 && data.file_name[1] == b'.' as u16) {
            let name = String::from_utf16_lossy(&data.file_name[..end]);
            let is_dir = data.attributes & ATTR_DIRECTORY != 0;
            out.push(Scanned {
                path: PathBuf::from(format!("{}\\{}", path.trim_end_matches('\\'), name)),
                name,
                size: ((data.size_high as u64) << 32 | data.size_low as u64),
                mtime: filetime_to_unix(data.write_time),
                is_dir,
                is_reparse: data.attributes & ATTR_REPARSE_POINT != 0,
            });
        }
        let more = unsafe { FindNextFileW(handle, &mut data) };
        if more == 0 {
            break;
        }
    }
    unsafe {
        FindClose(handle);
    }
    out
}

/// 遍历整卷（无管理员权限时的退路）：按层同步 + rayon 并行
pub fn walk_volume(letter: char, progress: &mut dyn FnMut(usize)) -> Vec<Entry> {
    let roots = vec![format!("{}:\\", letter)];
    let mut entries: Vec<Entry> = Vec::with_capacity(1 << 20);
    {
        let mut root = Entry::new(format!("{}:\\", letter), 0);
        root.is_dir = true;
        entries.push(root);
    }
    let mut frontier: Vec<(String, u32)> = vec![(roots[0].clone(), 0)];
    let mut last_report = 0usize;

    while !frontier.is_empty() {
        let scanned: Vec<(u32, Vec<Scanned>)> = frontier
            .par_iter()
            .map(|(p, idx)| (*idx, scan_dir(p)))
            .collect();

        let mut next: Vec<(String, u32)> = Vec::new();
        for (parent_idx, items) in scanned {
            for it in items {
                let idx = entries.len() as u32;
                let sub = it.is_dir && !it.is_reparse;
                let path = if sub { Some(it.path.to_string_lossy().to_string()) } else { None };
                let mut e = Entry::new(it.name, parent_idx);
                e.size = it.size;
                e.mtime = it.mtime;
                e.is_dir = it.is_dir;
                e.is_reparse = it.is_reparse;
                entries.push(e);
                if let Some(p) = path {
                    next.push((p, idx));
                }
            }
        }
        frontier = next;
        if entries.len() - last_report >= 100_000 {
            last_report = entries.len();
            progress(entries.len());
        }
    }
    progress(entries.len());
    entries
}
