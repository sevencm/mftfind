//! 二进制索引格式：单文件、可 mmap、零解析加载。
//!
//! 文件布局（全部小端）：
//! ```text
//! [0..16)   magic
//! [16..20)  version u32
//! [20..24)  count   u32          记录数
//! [24..32)  build_time i64       unix 秒
//! [32..36)  search_len u32       小写名字区字节数
//! [36..40)  display_len u32      原始名字区字节数
//! [40..44)  records_len u32      记录区字节数 (= count * 28)
//! [44..64)  保留
//! [64..)    records[count]，每条 28 字节：
//!              0..4   name_off u32   名字在各自区的偏移（两区同偏移，长度一致）
//!              4..6   name_len u16   名字字节数
//!              6..8   flags    u16   bit0=目录 bit1=重解析点 bit2=尺寸已知
//!              8..12  parent   u32   父记录下标，u32::MAX 表示根
//!             12..20  size     u64   已知时为字节数，未知时 u64::MAX
//!             20..28  mtime    i64   unix 秒
//! 之后依次是 search_blob（ASCII 小写名字）与 display_blob（原始大小写名字）
//! ```
//! 说明：小写化只处理 ASCII（UTF-8 下字节长度不变），因此两个名字区偏移完全对齐，
//! 每条记录只需存一份偏移。索引文件本身即 mmap 数据，进程启动无需反序列化。

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::OnceLock;

use memmap2::Mmap;

pub const MAGIC: &[u8; 16] = b"FSTSEARCH-INDEX1";
pub const VERSION: u32 = 1;
pub const HEADER_SIZE: usize = 64;
pub const RECORD_SIZE: usize = 28;

pub const FLAG_DIR: u16 = 1 << 0;
pub const FLAG_REPARSE: u16 = 1 << 1;
pub const FLAG_SIZE_KNOWN: u16 = 1 << 2;

pub const NO_PARENT: u32 = u32::MAX;

/// 索引构建期的中间记录
#[derive(Clone)]
pub struct Entry {
    pub name: String,
    pub parent: u32,
    pub size: u64,
    pub mtime: i64,
    pub is_dir: bool,
    pub is_reparse: bool,
}

impl Entry {
    pub fn new(name: impl Into<String>, parent: u32) -> Entry {
        Entry {
            name: name.into(),
            parent,
            size: u64::MAX,
            mtime: 0,
            is_dir: false,
            is_reparse: false,
        }
    }
}

/// ASCII 小写化（保持 UTF-8 字节长度不变）
pub fn lower_ascii(s: &str) -> Vec<u8> {
    s.as_bytes()
        .iter()
        .map(|b| if b.is_ascii_uppercase() { b + 32 } else { *b })
        .collect()
}

/// 写索引文件（先写临时文件再改名，避免半截文件）
pub fn write_index(path: &Path, entries: &[Entry], build_time: i64) -> io::Result<u64> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");

    let count = entries.len() as u32;
    let records_len = entries.len() * RECORD_SIZE;

    let mut search_blob: Vec<u8> = Vec::with_capacity(entries.len() * 24);
    let mut display_blob: Vec<u8> = Vec::with_capacity(entries.len() * 24);
    let mut records: Vec<u8> = Vec::with_capacity(records_len);

    // 复用同一个缓冲区做 ASCII 小写化，避免每条目一次堆分配
    let mut lower_buf: Vec<u8> = Vec::with_capacity(320);
    for e in entries {
        let off = search_blob.len() as u32;
        lower_buf.clear();
        lower_buf.extend(
            e.name
                .as_bytes()
                .iter()
                .map(|b| if b.is_ascii_uppercase() { b + 32 } else { *b }),
        );
        let len = lower_buf.len() as u16;
        search_blob.extend_from_slice(&lower_buf);
        display_blob.extend_from_slice(e.name.as_bytes());

        let mut flags = 0u16;
        if e.is_dir {
            flags |= FLAG_DIR;
        }
        if e.is_reparse {
            flags |= FLAG_REPARSE;
        }
        if e.size != u64::MAX {
            flags |= FLAG_SIZE_KNOWN;
        }

        records.extend_from_slice(&off.to_le_bytes());
        records.extend_from_slice(&len.to_le_bytes());
        records.extend_from_slice(&flags.to_le_bytes());
        records.extend_from_slice(&e.parent.to_le_bytes());
        records.extend_from_slice(&e.size.to_le_bytes());
        records.extend_from_slice(&e.mtime.to_le_bytes());
    }

    {
        let file = File::create(&tmp)?;
        let mut w = BufWriter::with_capacity(1 << 22, file);
        let mut header = [0u8; HEADER_SIZE];
        header[0..16].copy_from_slice(MAGIC);
        header[16..20].copy_from_slice(&VERSION.to_le_bytes());
        header[20..24].copy_from_slice(&count.to_le_bytes());
        header[24..32].copy_from_slice(&build_time.to_le_bytes());
        header[32..36].copy_from_slice(&(search_blob.len() as u32).to_le_bytes());
        header[36..40].copy_from_slice(&(display_blob.len() as u32).to_le_bytes());
        header[40..44].copy_from_slice(&(records_len as u32).to_le_bytes());
        w.write_all(&header)?;
        w.write_all(&records)?;
        w.write_all(&search_blob)?;
        w.write_all(&display_blob)?;
        w.flush()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok((HEADER_SIZE + records_len + search_blob.len() + display_blob.len()) as u64)
}

/// 已 mmap 的只读索引
pub struct Index {
    map: Mmap,
    pub path: String,
    pub letter: char,
    pub count: u32,
    pub build_time: i64,
    search_off: usize,
    search_len: usize,
    display_off: usize,
    offsets: OnceLock<Vec<u32>>,
}

fn u32_at(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
}
fn u16_at(buf: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([buf[at], buf[at + 1]])
}
fn u64_at(buf: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(buf[at..at + 8].try_into().unwrap())
}
fn i64_at(buf: &[u8], at: usize) -> i64 {
    i64::from_le_bytes(buf[at..at + 8].try_into().unwrap())
}

impl Index {
    pub fn open(path: &Path) -> io::Result<Index> {
        let file = File::open(path)?;
        let map = unsafe { Mmap::map(&file)? };
        if map.len() < HEADER_SIZE || &map[0..16] != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "索引文件格式不正确"));
        }
        let version = u32_at(&map, 16);
        if version != VERSION {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "索引文件版本不匹配"));
        }
        let count = u32_at(&map, 20);
        let build_time = i64_at(&map, 24);
        let search_len = u32_at(&map, 32) as usize;
        let display_len = u32_at(&map, 36) as usize;
        let records_len = u32_at(&map, 40) as usize;
        let search_off = HEADER_SIZE + records_len;
        let display_off = search_off + search_len;
        if display_off + display_len > map.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "索引文件被截断"));
        }
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let letter = name.chars().next().unwrap_or('C');
        Ok(Index {
            map,
            path: path.display().to_string(),
            letter,
            count,
            build_time,
            search_off,
            search_len,
            display_off,
            offsets: OnceLock::new(),
        })
    }

    #[inline]
    fn rec(&self, i: u32) -> usize {
        HEADER_SIZE + i as usize * RECORD_SIZE
    }

    #[inline]
    pub fn name_off(&self, i: u32) -> u32 {
        u32_at(&self.map, self.rec(i))
    }
    #[inline]
    pub fn name_len(&self, i: u32) -> u16 {
        u16_at(&self.map, self.rec(i) + 4)
    }
    #[inline]
    pub fn flags(&self, i: u32) -> u16 {
        u16_at(&self.map, self.rec(i) + 6)
    }
    #[inline]
    pub fn parent(&self, i: u32) -> u32 {
        u32_at(&self.map, self.rec(i) + 8)
    }
    #[inline]
    pub fn is_dir(&self, i: u32) -> bool {
        self.flags(i) & FLAG_DIR != 0
    }
    #[inline]
    pub fn is_reparse(&self, i: u32) -> bool {
        self.flags(i) & FLAG_REPARSE != 0
    }
    #[inline]
    pub fn size(&self, i: u32) -> Option<u64> {
        if self.flags(i) & FLAG_SIZE_KNOWN == 0 {
            return None;
        }
        Some(u64_at(&self.map, self.rec(i) + 12))
    }
    #[inline]
    pub fn mtime(&self, i: u32) -> i64 {
        i64_at(&self.map, self.rec(i) + 20)
    }

    /// 小写名字区整体切片（搜索直接在此扫描）
    pub fn search_blob(&self) -> &[u8] {
        &self.map[self.search_off..self.search_off + self.search_len]
    }

    /// 原始大小写的名字
    pub fn name(&self, i: u32) -> &str {
        let off = self.name_off(i) as usize;
        let len = self.name_len(i) as usize;
        std::str::from_utf8(&self.map[self.display_off + off..self.display_off + off + len])
            .unwrap_or("")
    }

    /// 小写名字（搜索用）
    pub fn lower_name(&self, i: u32) -> &[u8] {
        let off = self.name_off(i) as usize;
        let len = self.name_len(i) as usize;
        &self.map[self.search_off + off..self.search_off + off + len]
    }

    /// 名字偏移表（升序），用于「名字区字节位置 → 记录下标」二分定位
    pub fn offsets(&self) -> &Vec<u32> {
        self.offsets.get_or_init(|| {
            let mut v = Vec::with_capacity(self.count as usize);
            for i in 0..self.count {
                v.push(self.name_off(i));
            }
            v
        })
    }

    /// 记录下标 → 字节偏移 → 记录下标（memmem 命中位置换算）
    #[inline]
    pub fn record_at_offset(&self, byte_off: usize) -> u32 {
        let offs = self.offsets();
        match offs.binary_search(&(byte_off as u32)) {
            Ok(i) => i as u32,
            Err(i) => (i.saturating_sub(1)) as u32,
        }
    }

    /// 由父链拼出完整路径
    pub fn full_path(&self, i: u32) -> String {
        let mut parts: Vec<&str> = Vec::new();
        let mut cur = i;
        let mut guard = 0;
        loop {
            if cur == NO_PARENT || cur >= self.count || guard > 256 {
                break;
            }
            let rec = cur;
            let name = self.name(rec);
            let parent = self.parent(rec);
            if parent == rec || parent == NO_PARENT {
                // 根节点：名字本身即 "C:\"
                parts.push(name);
                break;
            }
            parts.push(name);
            cur = parent;
            guard += 1;
        }
        parts.reverse();
        let mut out = String::with_capacity(parts.iter().map(|p| p.len() + 1).sum());
        for (n, p) in parts.iter().enumerate() {
            if n == 0 {
                out.push_str(p);
            } else {
                if !out.ends_with('\\') {
                    out.push('\\');
                }
                out.push_str(p);
            }
        }
        out
    }
}
