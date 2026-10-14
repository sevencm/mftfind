//! mftfind —— Everything 风格的 Windows 极速文件搜索（Rust 引擎）
//!
//! 用法：
//!   mftfind                      启动图形界面
//!   mftfind gui                  启动图形界面
//!   mftfind index [C D ...]      建立/刷新索引（默认全部固定磁盘）
//!   mftfind index --walk C       强制使用并行遍历（不提权）
//!   mftfind search <关键词>      命令行搜索
//!   mftfind bench [C ...]        索引 + 搜索性能基准
//!   mftfind drives               列出磁盘、权限与索引状态
//!   mftfind mft [C ...]          直接探测 MFT 枚举（不走向遍历回退），用于定位问题
//!
//! 关于控制台：
//! 默认（不写 `windows_subsystem`）会链接成 `IMAGE_SUBSYSTEM_WINDOWS_CUI`，
//! 也就是「控制台程序」——只要启动就一定有一个黑窗口，从资源管理器双击更是如此。
//! 这里指定 `windows` 子系统，双击启动就不会再弹控制台；
//! 代价是进程拿不到标准句柄，需要在 main() 里用 `win::init_console` 修回来，
//! 否则 CLI 子命令和任何 println! 都会失效。
//! 用 `not(debug_assertions)` 限定，是为了让 debug 构建仍是控制台程序，
//! 方便 `cargo run -- bench` 这类调试时直接看输出。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod build;
mod format;
mod gui;
mod mft;
mod search;
mod walk;
mod win;

use std::time::Instant;

use format::Index;
use search::{parse_query, search, search_paths, SortBy};

/// 千分位格式化（Rust 格式串不支持 {:?} 的分组写法）
fn num(n: impl std::fmt::Display) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn usage() {
    println!(
        "mftfind —— 极速文件搜索（Rust 引擎 + FSCTL_ENUM_USN_DATA 直读 MFT）\n\
         用法:\n  \
         mftfind                            启动图形界面\n  \
         mftfind index [C D ...] [--walk]   建立索引（默认全部固定磁盘）\n  \
         mftfind search <关键词> [--limit N]\n  \
         mftfind bench [C ...]              性能基准\n  \
         mftfind drives                     磁盘 / 权限 / 索引状态\n  \
         mftfind mft [C ...]                直接探测 MFT 枚举（不走遍历回退），定位索引失败原因\n"
    );
}

fn parse_drives(args: &[String]) -> (Vec<char>, bool) {
    let mut drives = Vec::new();
    let mut force_walk = false;
    for a in args {
        if a == "--walk" {
            force_walk = true;
        } else if let Some(c) = a.chars().next() {
            if c.is_ascii_alphabetic() && a.len() <= 2 {
                drives.push(c.to_ascii_uppercase());
            }
        }
    }
    if drives.is_empty() {
        drives = win::fixed_drives();
    }
    (drives, force_walk)
}

fn cmd_index(args: &[String]) {
    let (drives, force_walk) = parse_drives(args);
    println!(
        "索引范围: {:?}  方式: {}",
        drives,
        if force_walk { "并行遍历" } else { "MFT（优先）" }
    );
    for letter in drives {
        let mut last = 0usize;
        let mut tick = |n: usize| {
            if n - last >= 200_000 {
                last = n;
                print!("\r    {}: 已解析 {} 条", letter, num(n));
                use std::io::Write;
                let _ = std::io::stdout().flush();
            }
        };
        match build::build_volume(letter, force_walk, &mut tick) {
            Ok(o) => println!(
                "\r  {}: {} 条 | 扫描 {:.2}s | 总计 {:.2}s | 方式 {}{} | 索引 {:.1} MB",
                letter,
                num(o.count),
                o.scan_secs,
                o.total_secs,
                if o.used_mft { "MFT" } else { "并行遍历" },
                match &o.fallback_reason {
                    Some(r) => format!("（MFT 回退原因：{}）", r),
                    None => String::new(),
                },
                o.bytes as f64 / 1_048_576.0
            ),
            Err(e) => println!("\r  {}: 失败 - {}", letter, e),
        }
    }
}

fn load_indexes() -> Vec<Index> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(build::index_dir()) {
        let mut paths: Vec<std::path::PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().map(|x| x == "fst").unwrap_or(false))
            .collect();
        paths.sort();
        for p in paths {
            match Index::open(&p) {
                Ok(ix) => out.push(ix),
                Err(e) => eprintln!("  无法加载 {}: {}", p.display(), e),
            }
        }
    }
    out
}

fn cmd_search(args: &[String]) {
    let mut limit = 30usize;
    let mut query_parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--limit" => {
                i += 1;
                limit = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(30);
            }
            other => query_parts.push(other.to_string()),
        }
        i += 1;
    }
    let query = query_parts.join(" ");
    if query.is_empty() {
        usage();
        return;
    }

    let t_load = Instant::now();
    let indexes = load_indexes();
    let load_ms = t_load.elapsed().as_secs_f64() * 1000.0;
    if indexes.is_empty() {
        println!("还没有索引，请先运行: mftfind index C");
        return;
    }
    let total_records: u64 = indexes.iter().map(|i| i.count as u64).sum();
    let (path_mode, tokens) = parse_query(&query);

    let t0 = Instant::now();
    let mut hits: Vec<(usize, u32)> = Vec::new();
    for (vi, ix) in indexes.iter().enumerate() {
        let r = if path_mode {
            search_paths(&indexes, vi, &tokens, 0)
        } else {
            search(ix, &tokens, 0)
        };
        hits.extend(r.into_iter().map(|r| (vi, r)));
    }
    let scan_ms = t0.elapsed().as_secs_f64() * 1000.0;

    println!(
        "索引 {} 个卷 / {} 条 | mmap 加载 {:.2} ms | 搜索 {:.2} ms | 命中 {} 条",
        indexes.len(),
        num(total_records),
        load_ms,
        scan_ms,
        num(hits.len())
    );
    for (vi, r) in hits.iter().take(limit) {
        let ix = &indexes[*vi];
        let full = ix.full_path(*r);
        let size = ix
            .size(*r)
            .filter(|&s| s != u64::MAX)
            .map(human)
            .unwrap_or_else(|| match std::fs::metadata(&full) {
                Ok(md) => human(md.len()),
                Err(_) => "-".to_string(),
            });
        println!(
            "  [{}] {:>10}  {}",
            if ix.is_dir(*r) { "DIR " } else { "FILE" },
            size,
            full
        );
    }
}

fn human(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{}{}", n, U[0])
    } else {
        format!("{:.1}{}", v, U[i])
    }
}

fn cmd_bench(args: &[String]) {
    let drives = if args.is_empty() {
        win::fixed_drives()
    } else {
        parse_drives(args).0
    };
    println!("=== 建索引 ===");
    for letter in drives {
        let mut last = 0usize;
        let mut tick = |n: usize| {
            if n - last >= 300_000 {
                last = n;
                print!("\r    {}: {}", letter, num(n));
                use std::io::Write;
                let _ = std::io::stdout().flush();
            }
        };
        match build::build_volume(letter, false, &mut tick) {
            Ok(o) => println!(
                "\r  {}: {} 条 | 扫描 {:.2}s | 总计 {:.2}s | {} | {:.1} MB | {} 条/秒",
                letter,
                num(o.count),
                o.scan_secs,
                o.total_secs,
                if o.used_mft { "MFT" } else { "遍历" },
                o.bytes as f64 / 1_048_576.0,
                num((o.count as f64 / o.scan_secs.max(0.001)) as u64)
            ),
            Err(e) => println!("\r  {}: 失败 - {}", letter, e),
        }
    }

    println!("\n=== 加载 + 搜索基准 ===");
    let t0 = Instant::now();
    let indexes = load_indexes();
    println!(
        "  mmap 加载 {} 个卷: {:.2} ms",
        indexes.len(),
        t0.elapsed().as_secs_f64() * 1000.0
    );
    let total: u64 = indexes.iter().map(|i| i.count as u64).sum();
    println!("  记录总数: {} 条", num(total));
    if indexes.is_empty() {
        return;
    }

    let queries = [
        "a", "log", "png", "python", "setup", "report", "文档", "cache", "config", "e",
    ];
    for q in queries {
        let tokens = search::tokenize(q);
        let mut best = f64::MAX;
        let mut hits = 0usize;
        for _ in 0..3 {
            let t = Instant::now();
            let mut n = 0usize;
            for ix in &indexes {
                n += search(ix, &tokens, 0).len();
            }
            hits = n;
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            if ms < best {
                best = ms;
            }
        }
        println!(
            "  查询 {:>8}  →  {:>7.2} ms  {:>9} 命中",
            format!("\"{}\"", q),
            best,
            num(hits)
        );
    }

    let tokens = search::tokenize("log");
    if let Some(ix) = indexes.first() {
        let mut h = search(ix, &tokens, 0);
        let t = Instant::now();
        search::sort_hits(ix, &mut h, SortBy::Name);
        println!(
            "  排序 {:>8}  →  {:>7.2} ms  ({} 条)",
            "\"log\"",
            t.elapsed().as_secs_f64() * 1000.0,
            num(h.len())
        );
    }
}

/// 直接探测 MFT 枚举：不经「回退遍历」逻辑，把真实结果和 Win32 错误码打出来。
/// 索引失败时先跑这个，一眼就能区分是权限问题、卷特性问题还是枚举逻辑问题。
fn cmd_mft(args: &[String]) {
    let mut letters: Vec<char> = args
        .iter()
        .filter_map(|a| a.chars().next())
        .filter(|c| c.is_ascii_alphabetic())
        .map(|c| c.to_ascii_uppercase())
        .collect();
    if letters.is_empty() {
        letters = win::fixed_drives();
    }
    println!("直接探测 FSCTL_ENUM_USN_DATA（需要管理员权限）：");
    for letter in letters {
        let t0 = Instant::now();
        match mft::enumerate_volume(letter, &mut |_| {}) {
            Ok(v) => println!(
                "  {}:\\  枚举成功：{} 条记录 / {:.2}s",
                letter,
                num(v.len()),
                t0.elapsed().as_secs_f64()
            ),
            Err(e) => {
                let why = match e.raw_os_error() {
                    Some(5) => "ERROR_ACCESS_DENIED —— 需要以管理员身份运行".to_string(),
                    Some(38) => {
                        "ERROR_HANDLE_EOF —— 出现在这里说明 EOF 未被当作正常出口（本应已修复）"
                            .to_string()
                    }
                    Some(1179) => "ERROR_JOURNAL_NOT_ACTIVE —— 该卷的 USN 日志未启用".to_string(),
                    Some(code) => format!("Win32 错误码 {}", code),
                    None => "非系统错误".to_string(),
                };
                println!("  {}:\\  失败：{} （{}）", letter, e, why);
            }
        }
    }
}

fn cmd_drives() {
    for letter in win::fixed_drives() {
        let (label, fs) = win::volume_info(letter);
        let mft = match win::Volume::open(letter) {
            Ok(_) => "可用（管理员权限，走 MFT 快路径）",
            Err(e) if win::is_error_access_denied(&e) => "拒绝访问（提权后可用 MFT）",
            Err(_) => "不可用",
        };
        let idx = build::index_path(letter);
        let idx_info = if idx.exists() {
            match Index::open(&idx) {
                Ok(ix) => format!(
                    "已索引 {} 条 / {:.1} MB",
                    num(ix.count),
                    std::fs::metadata(&idx).map(|m| m.len()).unwrap_or(0) as f64 / 1_048_576.0
                ),
                Err(_) => "索引损坏".to_string(),
            }
        } else {
            "未索引".to_string()
        };
        println!(
            "  {}:\\  {:<10} {:<5} | MFT: {} | {}",
            letter, label, fs, mft, idx_info
        );
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let first = args.first().map(|s| s.as_str());

    // 启动第一件事就修标准流：必须在任何 println! 之前，
    // 因为标准库会在首次使用时把句柄缓存起来，之后再 SetStdHandle 就晚了。
    // 只有 gui / 无参数（图形界面）走 Gui 模式，其余子命令都允许接回终端。
    win::init_console(match first {
        Some("gui") | None => win::ConsoleMode::Gui,
        _ => win::ConsoleMode::Cli,
    });

    match first {
        Some("index") => cmd_index(&args[1..]),
        Some("search") => cmd_search(&args[1..]),
        Some("bench") => cmd_bench(&args[1..]),
        Some("drives") => cmd_drives(),
        Some("mft") => cmd_mft(&args[1..]),
        Some("gui") | None => {
            if let Err(e) = gui::run() {
                eprintln!("界面启动失败: {}", e);
            }
        }
        Some("help") | Some("--help") | Some("-h") => usage(),
        Some(other) => {
            println!("未知命令: {}", other);
            usage();
        }
    }
}
