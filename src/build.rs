//! 索引构建：优先走 MFT（需管理员），权限不足自动回退并行遍历。

use std::io;
use std::path::PathBuf;
use std::time::Instant;

use crate::format::write_index;
use crate::i18n::Lang;
use crate::{mft, walk};

/// 索引目录：按 exe 的实际位置解析，而不是烧死编译期路径。
///
/// - 发布场景（把 exe 拷到任意目录）：索引放 exe 同级的 `index/`，随 exe 一起搬走，便携。
/// - 开发场景（exe 仍在 `target/<debug|release>/` 下）：索引放项目根的 `index/`，
///   避免索引藏进 target 里、更避免 `cargo clean` 把几百 MB 索引一起删掉。
pub fn index_dir() -> PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));

    let is_profile = |n: &std::ffi::OsStr| {
        n.eq_ignore_ascii_case("debug") || n.eq_ignore_ascii_case("release")
    };
    let in_target = exe_dir
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.eq_ignore_ascii_case("target"))
        .unwrap_or(false);

    if in_target {
        if let Some(profile) = exe_dir.file_name() {
            if is_profile(profile) {
                if let Some(root) = exe_dir.parent().and_then(|p| p.parent()) {
                    return root.join("index");
                }
            }
        }
    }
    exe_dir.join("index")
}

pub fn index_path(letter: char) -> PathBuf {
    index_dir().join(format!("{}.fst", letter))
}

pub struct BuildOutcome {
    pub count: usize,
    pub scan_secs: f64,
    pub total_secs: f64,
    pub used_mft: bool,
    /// MFT 不可用而回退到并行遍历时的原因（供状态栏显示）
    pub fallback_reason: Option<FallbackReason>,
    pub bytes: u64,
}

/// 回退到并行遍历的原因。
///
/// 刻意存枚举而不是现成的字符串：文案要在界面里按当前语言渲染，
/// 否则切到英文界面时会冒出中文的「MFT 未返回记录」。
#[derive(Clone, Debug)]
pub enum FallbackReason {
    /// MFT 枚举成功，但一条记录都没返回
    NoRecords,
    /// 底层调用失败（未提权时通常是拒绝访问）
    Error(String),
}

impl FallbackReason {
    pub fn text(&self, lang: Lang) -> String {
        match self {
            FallbackReason::NoRecords => lang.strings().fallback_no_records.to_string(),
            // 系统错误信息由 Windows 自行本地化，原样透出
            FallbackReason::Error(e) => e.clone(),
        }
    }
}

/// 建立某个卷的索引并落盘
pub fn build_volume(
    letter: char,
    force_walk: bool,
    progress: &mut dyn FnMut(usize),
) -> io::Result<BuildOutcome> {
    let t0 = Instant::now();
    let mut used_mft = false;
    let mut fallback_reason = None;
    let entries = if force_walk {
        walk::walk_volume(letter, progress)
    } else {
        match mft::enumerate_volume(letter, progress) {
            Ok(v) if !v.is_empty() => {
                used_mft = true;
                v
            }
            Ok(_) => {
                // 枚举到 0 条说明 MFT 这条路没拿到东西，别写一个空索引覆盖掉可用数据
                fallback_reason = Some(FallbackReason::NoRecords);
                walk::walk_volume(letter, progress)
            }
            Err(e) => {
                // 未提权时访问被拒是预期情况；其他 MFT 失败（如驱动/卷特性差异）
                // 也不该让整块盘索引失败——统一回退并行遍历，并记录原因供界面提示。
                fallback_reason = Some(FallbackReason::Error(e.to_string()));
                walk::walk_volume(letter, progress)
            }
        }
    };
    let scan_secs = t0.elapsed().as_secs_f64();
    let build_time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let bytes = write_index(&index_path(letter), &entries, build_time)?;
    Ok(BuildOutcome {
        count: entries.len(),
        scan_secs,
        total_secs: t0.elapsed().as_secs_f64(),
        used_mft,
        fallback_reason,
        bytes,
    })
}
