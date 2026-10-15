//! 通过 FSCTL_ENUM_USN_DATA 枚举整卷的 USN 记录（等价于遍历整张 MFT）。
//!
//! 这是 Windows 官方支持的接口，比手工解析 NTFS 内部结构稳健得多：
//! 一次 DeviceIoControl 就能取回上百条记录，整卷百万级文件通常 1~3 秒完成。
//! 注意：读取 USN 需要管理员权限，权限不足时上层会回退到并行目录遍历。

use std::collections::HashMap;
use std::io;

use crate::format::{Entry, NO_PARENT};
use crate::win::{Volume, FILETIME_UNIX_DIFF, FSCTL_ENUM_USN_DATA};

const ATTR_DIRECTORY: u32 = 0x0000_0010;
const ATTR_REPARSE_POINT: u32 = 0x0000_0400;

/// MFT 根目录记录号
const ROOT_RECORD_NUMBER: u64 = 5;
/// 读取缓冲：1 MiB 足以容纳上百条记录
const BUF_SIZE: usize = 1 << 20;

#[repr(C)]
#[derive(Clone, Copy)]
struct MftEnumDataV0 {
    start_file_reference_number: u64,
    low_usn: i64,
    high_usn: i64,
}

fn as_bytes<T: Copy>(v: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v as *const T as *const u8, std::mem::size_of::<T>()) }
}

#[inline]
fn record_number(frn: u64) -> u64 {
    frn & 0x0000_FFFF_FFFF_FFFF
}

struct RawRec {
    frn: u64,
    parent_frn: u64,
    attrs: u32,
    mtime: i64,
    name: String,
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}
fn i64_at(b: &[u8], at: usize) -> i64 {
    i64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// 解析 USN_RECORD_V2（固定部分 60 字节）
fn parse_v2(rec: &[u8], out: &mut Vec<RawRec>) {
    let frn = u64_at(rec, 8);
    let parent_frn = u64_at(rec, 16);
    let timestamp = i64_at(rec, 32);
    let attrs = u32_at(rec, 52);
    let name_len = u16_at(rec, 56) as usize;
    let name_off = u16_at(rec, 58) as usize;
    if name_len == 0 || name_off + name_len > rec.len() {
        return;
    }
    let units: Vec<u16> = rec[name_off..name_off + name_len]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let name = String::from_utf16_lossy(&units);
    out.push(RawRec {
        frn,
        parent_frn,
        attrs,
        mtime: filetime_to_unix(timestamp),
        name,
    });
}

/// FILETIME（100ns 粒度）→ Unix 秒。**0 表示「未知」并原样保留 0**。
///
/// FSCTL_ENUM_USN_DATA 返回的 USN_RECORD 里 TimeStamp 恒为 0：这条枚举路径出自
/// MFT 而非变更日志，微软只在 FSCTL_READ_FILE_USN_DATA 的文档里写明
/// TimeStamp / Reason / SourceInfo 为 invalid，实测 ENUM 同样如此
/// （本机 MFT 建的 5 个卷索引里，采样 100% 的记录时间戳都是 0）。
/// 若照常减去 FILETIME 偏移，0 会变成 -11644473600，看着像一个 1601 年的日期、
/// 实际是负数；界面按 `<= 0` 判未知，所以落盘时保持 0 更诚实。
fn filetime_to_unix(t: i64) -> i64 {
    if t <= 0 {
        0
    } else {
        t / 10_000_000 - FILETIME_UNIX_DIFF
    }
}

/// 枚举一卷的 MFT，构建索引条目。`progress` 会周期性收到已解析记录数。
pub fn enumerate_volume(
    letter: char,
    progress: &mut dyn FnMut(usize),
) -> io::Result<Vec<Entry>> {
    let volume = Volume::open(letter)?;
    let mut recs: Vec<RawRec> = Vec::with_capacity(1 << 20);
    let mut input = MftEnumDataV0 {
        start_file_reference_number: 0,
        low_usn: 0,
        high_usn: i64::MAX,
    };
    let mut buf = vec![0u8; BUF_SIZE];
    let mut last_report = 0usize;

    loop {
        // FSCTL_ENUM_USN_DATA 在枚举完毕时返回 ERROR_HANDLE_EOF（不是 0 字节的成功），
        // 所以这里必须把它识别成正常结束，不能当错误往外抛。
        let n = match volume.ioctl(FSCTL_ENUM_USN_DATA, as_bytes(&input), &mut buf) {
            Ok(n) => n,
            Err(e) if crate::win::is_error_eof(&e) => break,
            Err(e) => return Err(e),
        };
        if n < 8 {
            break; // 枚举结束（兼容返回 0 字节的驱动行为）
        }
        // 缓冲区开头 8 字节是下一轮的起始 FRN
        let next_start = u64_at(&buf, 0);
        let mut off = 8usize;
        while off + 60 <= n {
            let rec_len = u32_at(&buf, off) as usize;
            if rec_len < 60 || off + rec_len > n {
                break;
            }
            let major = u16_at(&buf, off + 4);
            if major == 2 {
                // 借用切片避免拷贝
                let slice = &buf[off..off + rec_len];
                parse_v2(slice, &mut recs);
            }
            // major == 3（128 位 FRN）在 V0 输入下不会出现
            off += rec_len;
        }
        if recs.len() - last_report >= 100_000 {
            last_report = recs.len();
            progress(recs.len());
        }
        if next_start == input.start_file_reference_number {
            break; // 没有推进，防止死循环
        }
        input.start_file_reference_number = next_start;
    }
    progress(recs.len());

    // ---- FRN → 记录下标 → 回填父节点 ----
    let mut frn_to_idx: HashMap<u64, u32> = HashMap::with_capacity(recs.len() * 2);
    let mut entries: Vec<Entry> = Vec::with_capacity(recs.len());
    let mut root_idx: u32 = u32::MAX;

    for r in &recs {
        let idx = entries.len() as u32;
        let is_root = record_number(r.frn) == ROOT_RECORD_NUMBER;
        let name = if is_root {
            format!("{}:\\", letter)
        } else {
            r.name.clone()
        };
        let mut e = Entry::new(name, NO_PARENT);
        e.is_dir = r.attrs & ATTR_DIRECTORY != 0 || is_root;
        e.is_reparse = r.attrs & ATTR_REPARSE_POINT != 0;
        e.mtime = r.mtime;
        // MFT 枚举拿不到文件大小（USN 记录不含 $DATA 长度），
        // 索引阶段留空，界面按需对可见行调用 metadata 惰性补齐。
        e.size = u64::MAX;
        entries.push(e);
        frn_to_idx.insert(r.frn, idx);
        if is_root {
            root_idx = idx;
        }
    }

    for (i, r) in recs.iter().enumerate() {
        if record_number(r.frn) == ROOT_RECORD_NUMBER {
            entries[i].parent = i as u32;
            continue;
        }
        entries[i].parent = match frn_to_idx.get(&r.parent_frn) {
            Some(&p) => p,
            None => NO_PARENT,
        };
    }

    // 根记录缺失时（罕见）补一个虚拟根，保证路径拼接正常
    if root_idx == u32::MAX {
        let mut root = Entry::new(format!("{}:\\", letter), entries.len() as u32);
        root.is_dir = true;
        let ridx = entries.len() as u32;
        entries.push(root);
        for e in entries.iter_mut() {
            if e.parent == NO_PARENT {
                e.parent = ridx;
            }
        }
        if let Some(last) = entries.last_mut() {
            last.parent = ridx;
        }
    }

    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 枚举路径拿不到时间戳（TimeStamp 恒为 0），落盘时必须保持 0 这个
    /// 「未知」标记。早先直接减 FILETIME 偏移，0 会变成 -11644473600——
    /// 一个看着像 1601 年、实际为负的数，界面只好整列留空。
    #[test]
    fn zero_timestamp_stays_zero() {
        assert_eq!(filetime_to_unix(0), 0);
        assert_eq!(filetime_to_unix(-1), 0, "负数同样视为未知");
    }

    /// 正常 FILETIME 要能换算成 Unix 秒（1 秒 = 10^7 个 100ns 单位）
    #[test]
    fn filetime_converts_to_unix_seconds() {
        let unix = 1_700_000_000i64;
        let filetime = (unix + FILETIME_UNIX_DIFF) * 10_000_000;
        assert_eq!(filetime_to_unix(filetime), unix);
    }
}
