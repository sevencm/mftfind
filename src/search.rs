//! 搜索核心：在 mmap 的小写名字区上用 C 级 memmem 扫描，大索引再按块并行。
//!
//! 关键点在于「不逐条比较」——整块名字区是一段连续内存，扫描速度接近内存带宽；
//! 命中位置通过名字偏移表二分换算成记录下标，多关键词取交集。

use memchr::memmem;
use rayon::prelude::*;

use crate::format::Index;

/// 单块并行阈值：超过 4 MiB 的名字区分块并行扫描
const CHUNK: usize = 4 << 20;

/// 把查询串切成小写 token（多关键词空格分隔）
pub fn tokenize(query: &str) -> Vec<Vec<u8>> {
    query
        .split_whitespace()
        .map(|t| {
            t.as_bytes()
                .iter()
                .map(|b| if b.is_ascii_uppercase() { b + 32 } else { *b })
                .collect::<Vec<u8>>()
        })
        .filter(|t: &Vec<u8>| !t.is_empty())
        .collect()
}

/// 结果排序方式
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SortBy {
    /// 名称（不区分大小写）
    Name,
    /// 修改时间（新 → 旧）
    Modified,
    /// 大小（大 → 小，未知尺寸排最后）
    Size,
    /// 目录优先，再按名称
    Kind,
}

/// 带卷号的命中项
#[derive(Clone, Copy)]
pub struct Hit {
    pub vol: usize,
    pub idx: u32,
}

/// 跨卷排序：比较时按卷号取对应索引
pub fn sort_hits_multi(indexes: &[Index], hits: &mut [Hit], by: SortBy) {
    hits.sort_unstable_by(|a, b| {
        let ia = &indexes[a.vol];
        let ib = &indexes[b.vol];
        match by {
            SortBy::Name => ia.lower_name(a.idx).cmp(ib.lower_name(b.idx)),
            SortBy::Modified => ib.mtime(b.idx).cmp(&ia.mtime(a.idx)),
            SortBy::Size => {
                let sa = ia.size(a.idx).unwrap_or(u64::MAX);
                let sb = ib.size(b.idx).unwrap_or(u64::MAX);
                sb.cmp(&sa)
            }
            SortBy::Kind => ib
                .is_dir(b.idx)
                .cmp(&ia.is_dir(a.idx))
                .then_with(|| ia.lower_name(a.idx).cmp(ib.lower_name(b.idx))),
        }
    });
}

/// 在单个卷索引上搜索
pub fn search(index: &Index, tokens: &[Vec<u8>], limit: usize) -> Vec<u32> {
    if tokens.is_empty() {
        return Vec::new();
    }
    let blob = index.search_blob();
    let mut sets: Vec<Vec<u32>> = tokens
        .par_iter()
        .map(|t| scan_token(index, blob, t))
        .collect();

    // 取最小的结果集做基准，逐个与其他集合求交
    sets.sort_by_key(|s| s.len());
    let mut result = sets[0].clone();
    result.sort_unstable();
    result.dedup();
    for other in &sets[1..] {
        if result.is_empty() {
            break;
        }
        let mut o = other.clone();
        o.sort_unstable();
        o.dedup();
        result.retain(|r| o.binary_search(r).is_ok());
    }
    if limit > 0 && result.len() > limit {
        result.truncate(limit);
    }
    result
}

/// 扫描单个 token，返回命中的记录下标（可能重复，调用方去重）
fn scan_token(index: &Index, blob: &[u8], token: &[u8]) -> Vec<u32> {
    let collect = |start: usize, end: usize| -> Vec<u32> {
        let mut v = Vec::new();
        for pos in memmem::find_iter(&blob[start..end], token) {
            v.push(index.record_at_offset(start + pos));
        }
        v
    };

    if blob.len() <= CHUNK || token.len() > 128 {
        let mut v = collect(0, blob.len());
        v.sort_unstable();
        v.dedup();
        return v;
    }

    // 分块并行：块间留 token_len-1 的重叠，避免切断跨块命中
    let overlap = token.len() - 1;
    let ranges: Vec<(usize, usize)> = (0..blob.len())
        .step_by(CHUNK)
        .map(|s| (s, (s + CHUNK + overlap).min(blob.len())))
        .collect();
    let mut v: Vec<u32> = ranges
        .par_iter()
        .flat_map_iter(|&(s, e)| collect(s, e))
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// 完整路径索引（路径搜索时才惰性构建）
pub struct PathSearch {
    blob: Vec<u8>,
    offsets: Vec<u32>,
}

/// 路径区缓存：键为各索引 (count, build_time) 的组合，重建索引后自动失效
static PATH_BLOBS: std::sync::Mutex<Option<(u64, Vec<PathSearch>)>> = std::sync::Mutex::new(None);

fn index_signature(indexes: &[Index]) -> u64 {
    indexes.iter().fold(0u64, |acc, ix| {
        acc.wrapping_mul(31)
            .wrapping_add(ix.count as u64 ^ ((ix.build_time as u64) << 20))
    })
}

/// 为一批索引惰性构建路径区（首次调用约 0.2~0.5 秒/百万条）
pub fn ensure_path_blobs(indexes: &[Index]) {
    let sig = index_signature(indexes);
    let mut guard = PATH_BLOBS.lock().unwrap();
    if let Some((cached, _)) = guard.as_ref() {
        if *cached == sig {
            return;
        }
    }
    let built: Vec<PathSearch> = indexes
        .iter()
        .map(|idx| {
            let mut blob = Vec::with_capacity(idx.count as usize * 48);
            let mut offsets = Vec::with_capacity(idx.count as usize);
            for r in 0..idx.count {
                offsets.push(blob.len() as u32);
                let p = idx.full_path(r);
                blob.extend(p.as_bytes().iter().map(|b| {
                    if b.is_ascii_uppercase() {
                        b + 32
                    } else {
                        *b
                    }
                }));
            }
            PathSearch { blob, offsets }
        })
        .collect();
    *guard = Some((sig, built));
}

/// 在路径区中搜索（用于带路径的查询）
pub fn search_paths(indexes: &[Index], vol: usize, tokens: &[Vec<u8>], limit: usize) -> Vec<u32> {
    ensure_path_blobs(indexes);
    let guard = PATH_BLOBS.lock().unwrap();
    let ps = match guard.as_ref().and_then(|(_, v)| v.get(vol)) {
        Some(ps) => ps,
        None => return Vec::new(),
    };
    let mut sets: Vec<Vec<u32>> = tokens
        .par_iter()
        .map(|t| {
            let mut v: Vec<u32> = memmem::find_iter(&ps.blob, t)
                .map(|pos| {
                    let pos = pos as u32;
                    match ps.offsets.binary_search(&pos) {
                        Ok(i) => i as u32,
                        Err(i) => i.saturating_sub(1) as u32,
                    }
                })
                .collect();
            v.sort_unstable();
            v.dedup();
            v
        })
        .collect();
    if sets.is_empty() {
        return Vec::new();
    }
    sets.sort_by_key(|s| s.len());
    let pivot = sets.remove(0);
    let mut result: Vec<u32> = if sets.is_empty() {
        pivot
    } else {
        let rest = sets;
        pivot
            .into_par_iter()
            .filter(|r| rest.iter().all(|s| s.binary_search(r).is_ok()))
            .collect()
    };
    result.sort_unstable();
    if limit > 0 && result.len() > limit {
        result.truncate(limit);
    }
    result
}

/// 按要求排序命中的记录下标
pub fn sort_hits(index: &Index, hits: &mut [u32], by: SortBy) {
    match by {
        SortBy::Name => hits.sort_unstable_by(|&a, &b| index.lower_name(a).cmp(index.lower_name(b))),
        SortBy::Modified => hits.sort_unstable_by(|&a, &b| index.mtime(b).cmp(&index.mtime(a))),
        SortBy::Size => hits.sort_unstable_by(|&a, &b| {
            let sa = index.size(a).unwrap_or(u64::MAX);
            let sb = index.size(b).unwrap_or(u64::MAX);
            sb.cmp(&sa)
        }),
        SortBy::Kind => hits.sort_unstable_by(|&a, &b| {
            index
                .is_dir(b)
                .cmp(&index.is_dir(a))
                .then_with(|| index.lower_name(a).cmp(index.lower_name(b)))
        }),
    }
}

/// 解析查询：返回 (是否需要路径匹配, token 列表)
pub fn parse_query(query: &str) -> (bool, Vec<Vec<u8>>) {
    let path_mode = query.contains('\\') || query.contains('/');
    (path_mode, tokenize(query))
}
