//! egui 界面：顶部搜索框 + 虚拟滚动结果表 + 右键菜单 + 索引导航。
//!
//! 交互要点：
//! - 输入即搜：查询变化后丢给后台线程，避免阻塞界面（结果通过 Arc<Mutex<>> 回传）
//! - 结果表用 ScrollArea::show_rows 做虚拟滚动，百万级结果也不卡
//! - MFT 索引不含文件大小，只对「当前可见行」惰性调用 metadata 补齐并缓存

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use eframe::egui;

use crate::build;
use crate::format::Index;
use crate::i18n::{self, Lang};
use crate::search::{parse_query, search, search_paths, sort_hits_multi, Hit, SortBy};
use crate::win;

/// 单次搜索最多保留的命中数（界面只显示可见行）
const MAX_HITS: usize = 20_000;
const ROW_HEIGHT: f32 = 20.0;

/// 菜单里标记当前项用的勾，以及不选中时占位的等宽空白。
///
/// 这个勾**依赖 `install_fonts` 里的符号字体回退**：egui 0.36 自带的三个字体
/// 都不含 `✓`（实测只覆盖 `☐ ● • · √ ★ ☆ → ⛃ ☜`），少了 Segoe UI Symbol
/// 就会显示成豆腐块空框。有单测锁住
/// （`menu_check_mark_is_actually_renderable`）。
const CHECK_MARK: &str = "✓ ";
const NO_MARK: &str = "   ";

/// 搜索框右侧控件（路径勾选框 + 排序下拉）预留的宽度。
/// 固定值避免切语言时搜索框跳动；语言选择在菜单栏，不占这行宽度。
const CONTROLS_WIDTH: f32 = 230.0;

struct Meta {
    ms: f64,
    total: usize,
    generation: u64,
    path_ready_ms: f64,
}

/// 一行结果里需要补齐的两列，都是**渲染好的字符串**，直接进表格。
#[derive(Clone)]
struct RowMeta {
    size: String,
    time: String,
}

/// 补齐索引里缺失的大小与修改时间。
///
/// MFT 枚举路径拿不到这两项（USN_RECORD 里既没有文件大小也没有时间戳），
/// 所以对**当前可见行**调一次 `fs::metadata` 同时取回两者并缓存——
/// 一次 stat 补两列，而不是各来一次。索引里已有的值不碰磁盘。
///
/// 写成自由函数而不是 `&mut self` 方法：调用处同时持有 `&self.indexes`
/// （借了 self 的一个字段），只有分开借字段才不会冲突。
fn row_meta(
    cache: &mut HashMap<(usize, u32), RowMeta>,
    ix: &Index,
    vol: usize,
    idx: u32,
    path: &str,
) -> RowMeta {
    if let Some(m) = cache.get(&(vol, idx)) {
        return m.clone();
    }

    let indexed_size = ix.size(idx);
    let indexed_time = ix.mtime(idx);
    // 目录的大小一律显示 "-"：NTFS 里目录条目没有有意义的字节数，
    // 显示 "0 B" 会让人误以为文件夹是空的。文件则用索引里的值，
    // 缺了（MFT 路径）就去磁盘补。
    let is_dir = ix.is_dir(idx);
    let need_size = !is_dir && indexed_size.is_none();
    let need_time = indexed_time <= 0;

    let mut out = RowMeta {
        size: match indexed_size {
            Some(s) if !is_dir && s != u64::MAX => human_size(s),
            _ => UNKNOWN.to_string(),
        },
        time: fmt_time(indexed_time),
    };

    // 只有确实缺字段时才碰磁盘
    if need_size || need_time {
        if let Some((size, time)) = meta_from_disk(path, is_dir) {
            if need_size {
                if let Some(s) = size {
                    out.size = s;
                }
            }
            if need_time {
                if let Some(t) = time {
                    out.time = t;
                }
            }
        }
        // 读不到（权限/已删除）就保持 "-"：比留空更明确
    }

    cache.insert((vol, idx), out.clone());
    out
}

/// 从文件系统读一行的（大小文本，时间文本）。两项都是 `Option`：
/// 目录没有有意义的大小，时间也可能取不到。整条路径都失败时返回 `None`。
///
/// 抽出来是为了能测——`row_meta` 需要 `&Index`（要 mmap 一个 .fst），
/// 这一段只依赖路径，可以直接对临时文件断言。
fn meta_from_disk(path: &str, is_dir: bool) -> Option<(Option<String>, Option<String>)> {
    let md = std::fs::metadata(path).ok()?;
    let size = if is_dir {
        None
    } else {
        Some(human_size(md.len()))
    };
    let time = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| fmt_time(d.as_secs() as i64))
        .filter(|s| !s.is_empty());
    Some((size, time))
}

/// 读不到时的占位符
const UNKNOWN: &str = "-";

/// 状态栏消息。**存结构化数据而不是渲染好的字符串**——
/// 字符串在生成那一刻就固定了语言，切换语言后旧文案不会跟着变
/// （首次切语言的 bug 根因）。绘制时才用当前语言渲染。
enum Status {
    Elevated,
    NotElevated,
    Reloaded,
    Copied,
    OpenFailed(String),
    RevealFailed(String),
}

impl Status {
    fn text(&self, s: &i18n::Strings) -> String {
        match self {
            Status::Elevated => s.status_elevated.to_string(),
            Status::NotElevated => s.status_not_elevated.to_string(),
            Status::Reloaded => s.status_reloaded.to_string(),
            Status::Copied => s.status_copied.to_string(),
            Status::OpenFailed(e) => s.open_failed(e),
            Status::RevealFailed(e) => s.reveal_failed(e),
        }
    }
}

/// 单卷索引结果（供状态栏按当前语言渲染）
struct VolumeDone {
    letter: char,
    count: usize,
    scan_secs: f64,
    total_secs: f64,
    used_mft: bool,
    fallback: Option<build::FallbackReason>,
    /// 全部卷索引结束后置 true，状态栏追加「可开始搜索」
    ready: bool,
}

/// 索引进度。跨线程共享（后台线程写、界面线程读），同样存结构化数据。
enum IndexStatus {
    None,
    Start(char),
    Progress(char, usize),
    Done(VolumeDone),
    Failed { letter: char, error: String },
}

impl IndexStatus {
    fn text(&self, s: &i18n::Strings, lang: Lang) -> Option<String> {
        let body = match self {
            IndexStatus::None => return None,
            IndexStatus::Start(letter) => s.indexing_start(*letter),
            IndexStatus::Progress(letter, n) => s.indexing_progress(*letter, *n),
            IndexStatus::Done(d) => {
                let fallback = d
                    .fallback
                    .as_ref()
                    .map(|r| s.mft_fallback(&r.text(lang)))
                    .unwrap_or_default();
                let method = if d.used_mft { s.method_mft } else { s.method_walk };
                s.indexing_done(
                    d.letter,
                    d.count,
                    d.scan_secs,
                    d.total_secs,
                    method,
                    &fallback,
                )
            }
            IndexStatus::Failed { letter, error } => s.indexing_failed(*letter, error),
        };
        let ready = matches!(self, IndexStatus::Done(d) if d.ready);
        Some(if ready {
            format!("{}{}", body, s.ready_to_search)
        } else {
            body
        })
    }
}

struct DriveItem {
    letter: char,
    label: String,
    fs: String,
    indexed: bool,
    checked: bool,
}

pub struct App {
    indexes: Arc<Vec<Index>>,
    query: String,
    last_query: String,
    path_mode: bool,
    sort: SortBy,
    hits: Arc<Mutex<Vec<Hit>>>,
    meta: Arc<Mutex<Meta>>,
    generation: u64,
    /// 可见行的惰性补齐缓存（大小 + 修改时间），键为 (卷号, 记录下标)
    meta_cache: HashMap<(usize, u32), RowMeta>,
    selected: Option<usize>,
    status: Status,
    index_status: Arc<Mutex<IndexStatus>>,
    indexing: bool,
    /// 后台索引线程完成时置位。界面线程每帧检查它：
    /// 收到后装载新索引并重跑搜索——**不这么做的话，首次索引完必须手动
    /// 点「重新加载索引」才能搜到东西**（界面手里的还是启动时的旧 Arc）。
    indexing_done: Arc<AtomicBool>,
    drives: Vec<DriveItem>,
    show_scope: bool,
    elevated: bool,
    total_records: usize,
    focus_search: bool,
    /// 当前界面语言。改动后立即生效并写入配置文件。
    lang: Lang,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, lang: Lang) -> App {
        install_fonts(&cc.egui_ctx);
        let indexes = Arc::new(load_indexes());
        let total_records = indexes.iter().map(|i| i.count as usize).sum();
        let elevated = win::is_elevated();
        let mut app = App {
            indexes,
            query: String::new(),
            last_query: String::new(),
            path_mode: false,
            sort: SortBy::Name,
            hits: Arc::new(Mutex::new(Vec::new())),
            meta: Arc::new(Mutex::new(Meta {
                ms: 0.0,
                total: 0,
                generation: 0,
                path_ready_ms: 0.0,
            })),
            generation: 0,
            meta_cache: HashMap::new(),
            selected: None,
            status: if elevated {
                Status::Elevated
            } else {
                Status::NotElevated
            },
            index_status: Arc::new(Mutex::new(IndexStatus::None)),
            indexing: false,
            indexing_done: Arc::new(AtomicBool::new(false)),
            drives: list_drive_items(),
            show_scope: false,
            elevated,
            total_records,
            focus_search: true,
            lang,
        };
        app.spawn_search(&cc.egui_ctx);
        app
    }

    /// 后台线程执行搜索
    fn spawn_search(&mut self, ctx: &egui::Context) {
        self.generation += 1;
        let generation = self.generation;
        let query = self.query.clone();
        let indexes = self.indexes.clone();
        let hits = self.hits.clone();
        let meta = self.meta.clone();
        let path_mode = self.path_mode;
        let sort = self.sort;
        let ctx = ctx.clone();

        std::thread::spawn(move || {
            let t0 = Instant::now();
            let (auto_path, tokens) = parse_query(&query);
            let use_path = path_mode || auto_path;
            let mut all: Vec<Hit> = Vec::new();
            let mut path_ms = 0.0;
            if !tokens.is_empty() {
                if use_path {
                    let tp = Instant::now();
                    for (vol, _ix) in indexes.iter().enumerate() {
                        let r = search_paths(&indexes, vol, &tokens, MAX_HITS);
                        all.extend(r.into_iter().map(|idx| Hit { vol, idx }));
                    }
                    path_ms = tp.elapsed().as_secs_f64() * 1000.0;
                } else {
                    for (vol, ix) in indexes.iter().enumerate() {
                        let r = search(ix, &tokens, MAX_HITS);
                        all.extend(r.into_iter().map(|idx| Hit { vol, idx }));
                    }
                }
                let total = all.len();
                sort_hits_multi(&indexes, &mut all, sort);
                let mut m = meta.lock().unwrap();
                m.ms = t0.elapsed().as_secs_f64() * 1000.0;
                m.total = total;
                m.generation = generation;
                m.path_ready_ms = path_ms;
                *hits.lock().unwrap() = all;
            } else {
                let mut m = meta.lock().unwrap();
                m.ms = 0.0;
                m.total = 0;
                m.generation = generation;
                hits.lock().unwrap().clear();
            }
            ctx.request_repaint();
        });
    }

    fn reload_indexes(&mut self) {
        self.indexes = Arc::new(load_indexes());
        self.total_records = self.indexes.iter().map(|i| i.count as usize).sum();
        self.meta_cache.clear();
        self.drives = list_drive_items();
    }

    fn start_indexing(&mut self, letters: Vec<char>, ctx: &egui::Context, force_walk: bool) {
        if self.indexing || letters.is_empty() {
            return;
        }
        self.indexing = true;
        let status = self.index_status.clone();
        let done = self.indexing_done.clone();
        let ctx = ctx.clone();
        let elevated = self.elevated;
        done.store(false, Ordering::Relaxed);
        std::thread::spawn(move || {
            for letter in letters {
                *status.lock().unwrap() = IndexStatus::Start(letter);
                let mut last = 0usize;
                let res = build::build_volume(letter, force_walk && !elevated, &mut |n| {
                    if n - last >= 100_000 {
                        last = n;
                        *status.lock().unwrap() = IndexStatus::Progress(letter, n);
                    }
                });
                match res {
                    Ok(o) => {
                        *status.lock().unwrap() = IndexStatus::Done(VolumeDone {
                            letter,
                            count: o.count,
                            scan_secs: o.scan_secs,
                            total_secs: o.total_secs,
                            used_mft: o.used_mft,
                            fallback: o.fallback_reason,
                            ready: false,
                        });
                    }
                    Err(e) => {
                        *status.lock().unwrap() = IndexStatus::Failed {
                            letter,
                            error: e.to_string(),
                        };
                    }
                }
                ctx.request_repaint();
            }
            // 全部卷结束，在最终状态上标记「可开始搜索」，
            // 并通知界面线程装载新索引
            if let IndexStatus::Done(d) = &mut *status.lock().unwrap() {
                d.ready = true;
            }
            done.store(true, Ordering::Relaxed);
            ctx.request_repaint();
        });
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
            if let Ok(ix) = Index::open(&p) {
                out.push(ix);
            }
        }
    }
    out
}

fn list_drive_items() -> Vec<DriveItem> {
    win::fixed_drives()
        .into_iter()
        .map(|letter| {
            let (label, fs) = win::volume_info(letter);
            let indexed = build::index_path(letter).exists();
            DriveItem {
                letter,
                label,
                fs,
                indexed,
                checked: indexed,
            }
        })
        .collect()
}

/// 装配界面字体：egui 自带字体 + 系统符号/CJK 字体。
///
/// 回退顺序（egui 按家族列表逐字查找字形，取第一个命中的）：
///   Ubuntu-Light → NotoEmoji → emoji-icon-font → Segoe UI Symbol → 中文字体
///
/// - 拉丁字母走 egui 自带字体（比黑体好看），中文只在缺字时才回退，
///   所以 CJK 字体是 **push 到末尾** 而不是插到开头。插到开头会让英文界面
///   里所有字母都用黑体渲染。
/// - 符号字体（Segoe UI Symbol）是必需的：egui 0.36 自带的三个字体
///   **不含** `✔ ✓ ☑ ☀` 这类符号（实测覆盖范围里只有 `☐ ● • · √ ★ ☆ → ⛃ ☜`），
///   菜单里的勾会直接变成豆腐块空框。有单测锁住这一点
///   （`menu_check_mark_is_actually_renderable`）。
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    // 顺序即优先级：符号字体排在中文字体之前，各取所长
    add_fallback(
        &mut fonts,
        "symbol",
        &["C:/Windows/Fonts/seguisym.ttf", "C:/Windows/Fonts/segoeui.ttf"],
    );
    add_fallback(
        &mut fonts,
        "cjk",
        &[
            "C:/Windows/Fonts/simhei.ttf",
            "C:/Windows/Fonts/Deng.ttf",
            "C:/Windows/Fonts/msyh.ttc",
            "C:/Windows/Fonts/simsun.ttc",
        ],
    );
    ctx.set_fonts(fonts);
}

/// 从候选路径里取第一个能读到的字体，追加到两个家族的回退链末尾。
fn add_fallback(fonts: &mut egui::FontDefinitions, name: &str, candidates: &[&str]) {
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            fonts
                .font_data
                .insert(name.to_owned(), Arc::new(egui::FontData::from_owned(bytes)));
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts
                    .families
                    .entry(family)
                    .or_default()
                    .push(name.to_owned());
            }
            return;
        }
    }
}

fn human_size(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", n, U[0])
    } else {
        format!("{:.1} {}", v, U[i])
    }
}

/// Unix 秒 → `YYYY/MM/DD HH:MM`。`ts <= 0` 是本项目约定的「时间未知」，
/// 返回空串，由调用方决定显示什么占位。
fn fmt_time(ts: i64) -> String {
    if ts <= 0 {
        return String::new();
    }
    // 简易时间格式化：不引入 chrono，用 SystemTime 反算
    let secs = ts as u64;
    let days = secs / 86400;
    let rem = secs % 86400;
    let (h, m) = (rem / 3600, (rem % 3600) / 60);
    // 1970-01-01 起的天数 → 年月日（民用历算法）
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{:04}/{:02}/{:02} {:02}:{:02}", y, mo, d, h, m)
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

impl eframe::App for App {
    // eframe 0.36 起 App trait 从 update(ctx) 改为 ui(ui, frame)：
    // 由框架提供一个根 Ui，面板挂在它上面（Panel/CentralPanel::show 现在接收 &mut Ui）。
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // 本帧用到的文案表。切换语言后当前帧仍用旧表，下一帧生效。
        let s = self.lang.strings();

        // ---------- 快捷键 ----------
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.query.clear();
        }
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::F)) {
            self.focus_search = true;
        }

        // ---------- 索引完成：装载新索引并重搜 ----------
        // 后台线程只写 indexing_done，装载必须发生在界面线程（self.indexes
        // 与搜索线程共享）。放在面板之前，spinner 与状态栏当帧就更新。
        if self.indexing && self.indexing_done.load(Ordering::Relaxed) {
            self.indexing = false;
            self.reload_indexes();
            self.spawn_search(&ctx);
        }

        // ---------- 菜单栏 ----------
        egui::Panel::top("menu").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                let before = self.lang;
                ui.menu_button(s.language, |ui| {
                    for lang in Lang::ALL {
                        // 当前语言打勾；选项文字用各自语言书写，看不懂当前界面语言
                        // 的人也能在菜单里认出自己的那一项
                        let mark = if lang == self.lang { CHECK_MARK } else { NO_MARK };
                        ui.selectable_value(
                            &mut self.lang,
                            lang,
                            format!("{mark}{}", lang.endonym()),
                        );
                    }
                });
                if self.lang != before {
                    i18n::save(self.lang);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Title(window_title(self.lang)));
                }
            });
        });

        // ---------- 顶部搜索栏 ----------
        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(s.search_label);
                // 给「路径」勾选框 + 排序下拉留出固定宽度
                let desired = ui.available_width() - CONTROLS_WIDTH;
                let resp = ui.add_sized(
                    [desired.max(200.0), 24.0],
                    egui::TextEdit::singleline(&mut self.query).hint_text(s.search_hint),
                );
                if self.focus_search {
                    resp.request_focus();
                    self.focus_search = false;
                }
                ui.checkbox(&mut self.path_mode, s.path_mode);
                egui::ComboBox::from_id_salt("sort")
                    .selected_text(match self.sort {
                        SortBy::Name => s.sort_name,
                        SortBy::Modified => s.sort_modified,
                        SortBy::Size => s.sort_size,
                        SortBy::Kind => s.sort_kind,
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.sort, SortBy::Name, s.sort_name);
                        ui.selectable_value(&mut self.sort, SortBy::Modified, s.sort_modified);
                        ui.selectable_value(&mut self.sort, SortBy::Size, s.sort_size);
                        ui.selectable_value(&mut self.sort, SortBy::Kind, s.sort_kind);
                    });
            });
            ui.horizontal(|ui| {
                if ui.button(s.choose_scope).clicked() {
                    self.drives = list_drive_items();
                    self.show_scope = true;
                }
                if ui.button(s.reload_indexes).clicked() {
                    self.reload_indexes();
                    self.status = Status::Reloaded;
                }
                if !self.elevated && ui.button(s.restart_admin).clicked() {
                    let _ = win::restart_as_admin("");
                }
                // 按当前语言渲染；后台线程随时可能改写，绘制完立刻释放锁
                let st = {
                    let st = self.index_status.lock().unwrap();
                    st.text(s, self.lang)
                };
                if let Some(text) = st {
                    ui.label(egui::RichText::new(text).color(egui::Color32::from_rgb(200, 150, 60)));
                }
                if self.indexing {
                    ui.spinner();
                }
            });
            ui.add_space(4.0);
        });

        // ---------- 底部状态栏 ----------
        egui::Panel::bottom("bottom").show(ui, |ui| {
            let meta = self.meta.lock().unwrap();
            ui.horizontal(|ui| {
                ui.label(s.volumes_and_records(self.indexes.len(), self.total_records));
                ui.separator();
                if meta.total > 0 {
                    ui.label(s.hit_count(meta.total));
                    ui.label(s.search_ms(meta.ms));
                } else {
                    ui.label(s.type_to_search);
                }
                ui.separator();
                ui.label(self.status.text(s));
            });
        });

        // ---------- 索引范围窗口 ----------
        if self.show_scope {
            let mut open = self.show_scope;
            let mut start = false;
            let mut force_walk = false;
            egui::Window::new(s.scope_title)
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .show(&ctx, |ui| {
                    ui.label(s.scope_hint);
                    for d in self.drives.iter_mut() {
                        ui.horizontal(|ui| {
                            ui.checkbox(
                                &mut d.checked,
                                format!(
                                    "{}:\\  {} [{}] {}",
                                    d.letter,
                                    d.label,
                                    d.fs,
                                    if d.indexed { s.already_indexed } else { "" }
                                ),
                            );
                        });
                    }
                    ui.separator();
                    ui.horizontal(|ui| {
                        if ui.button(s.start_indexing).clicked() {
                            start = true;
                        }
                        ui.checkbox(&mut force_walk, s.force_walk);
                    });
                    ui.label(
                        egui::RichText::new(if self.elevated {
                            s.scope_elevated
                        } else {
                            s.scope_not_elevated
                        })
                        .small(),
                    );
                });
            self.show_scope = open;
            if start {
                let letters: Vec<char> = self
                    .drives
                    .iter()
                    .filter(|d| d.checked)
                    .map(|d| d.letter)
                    .collect();
                self.show_scope = false;
                self.start_indexing(letters, &ctx, force_walk);
            }
        }

        // ---------- 查询变化触发搜索 ----------
        if self.query != self.last_query {
            self.last_query = self.query.clone();
            self.spawn_search(&ctx);
        }

        // ---------- 结果表 ----------
        egui::CentralPanel::default().show(ui, |ui| {
            let row_h = ROW_HEIGHT;
            let avail = ui.available_width();
            let w_size = 90.0;
            let w_time = 130.0;
            let w_name = (avail * 0.32).clamp(140.0, 420.0);
            let w_path = (avail - w_name - w_size - w_time - 24.0).max(120.0);

            ui.horizontal(|ui| {
                ui.add_sized([w_name, row_h], egui::Label::new(egui::RichText::new(s.col_name).strong()));
                ui.add_sized([w_path, row_h], egui::Label::new(egui::RichText::new(s.col_path).strong()));
                ui.add_sized([w_size, row_h], egui::Label::new(egui::RichText::new(s.col_size).strong()));
                ui.add_sized([w_time, row_h], egui::Label::new(egui::RichText::new(s.col_modified).strong()));
            });
            ui.separator();

            let hit_list = self.hits.lock().unwrap().clone();
            let n = hit_list.len();

            let mut open_path: Option<(String, bool)> = None;
            let mut reveal: Option<String> = None;
            let mut copy: Option<String> = None;

            egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                .show_rows(ui, row_h, n, |ui, range| {
                    for i in range {
                        let h = hit_list[i];
                        let ix = &self.indexes[h.vol];
                        let is_dir = ix.is_dir(h.idx);
                        let full = ix.full_path(h.idx);
                        let name = ix.name(h.idx).to_string();

                        // 大小与修改时间：索引里缺的（MFT 路径两项都没有）
                        // 就对这一行 stat 一次补齐，结果进缓存
                        let row = row_meta(&mut self.meta_cache, ix, h.vol, h.idx, &full);
                        let size_text = row.size;

                        if self.selected == Some(i) {
                            let rect = ui.max_rect();
                            ui.painter().rect_filled(
                                egui::Rect::from_min_size(
                                    egui::pos2(rect.left(), rect.top()),
                                    egui::vec2(rect.width(), row_h),
                                ),
                                2.0,
                                ui.visuals().selection.bg_fill,
                            );
                        }

                        let resp = ui
                            .horizontal(|ui| {
                                let name_text = if is_dir {
                                    egui::RichText::new(format!("📁 {}", name)).color(egui::Color32::from_rgb(90, 150, 230))
                                } else {
                                    egui::RichText::new(name.clone())
                                };
                                ui.add_sized(
                                    [w_name, row_h],
                                    egui::Label::new(name_text).truncate(),
                                );
                                ui.add_sized(
                                    [w_path, row_h],
                                    egui::Label::new(
                                        egui::RichText::new(parent_of(&full)).weak(),
                                    )
                                    .truncate(),
                                );
                                ui.add_sized([w_size, row_h], egui::Label::new(size_text));
                                ui.add_sized([w_time, row_h], egui::Label::new(row.time));
                            })
                            .response
                            .interact(egui::Sense::click());

                        if resp.clicked() {
                            self.selected = Some(i);
                        }
                        if resp.double_clicked() {
                            open_path = Some((full.clone(), is_dir));
                        }
                        resp.context_menu(|ui| {
                            if ui.button(s.menu_open).clicked() {
                                open_path = Some((full.clone(), is_dir));
                            }
                            if ui.button(s.menu_reveal).clicked() {
                                reveal = Some(full.clone());
                            }
                            if ui.button(s.menu_copy).clicked() {
                                copy = Some(full.clone());
                            }
                        });
                    }
                });

            if let Some((p, d)) = open_path {
                if let Err(e) = win::open_path(&p, d) {
                    self.status = Status::OpenFailed(e.to_string());
                }
            }
            if let Some(p) = reveal {
                if let Err(e) = win::reveal_in_explorer(&p) {
                    self.status = Status::RevealFailed(e.to_string());
                }
            }
            if let Some(p) = copy {
                ctx.copy_text(p);
                self.status = Status::Copied;
            }
        });
    }
}

fn parent_of(path: &str) -> String {
    match path.rfind('\\') {
        Some(i) if i > 2 => path[..i].to_string(),
        Some(i) => path[..i + 1].to_string(),
        None => String::new(),
    }
}

/// 窗口标题，随语言变化
fn window_title(lang: Lang) -> String {
    format!(
        "mftfind {} - {}",
        env!("CARGO_PKG_VERSION"),
        lang.strings().window_title
    )
}

/// 128×128 RGBA 像素，由 `tools/make_icon.py` 生成，改图标先改那个脚本再重跑。
/// 嵌原始像素而不是 PNG：省掉 image 解码依赖，也不用每次启动解码一遍。
const APP_ICON: &[u8] = include_bytes!("../assets/icon.rgba");

fn app_icon() -> egui::IconData {
    // 尺寸写死在生成脚本里，这里用常量断言兜底，防止两个文件悄悄脱节。
    debug_assert_eq!(APP_ICON.len(), 128 * 128 * 4);
    egui::IconData {
        rgba: APP_ICON.to_vec(),
        width: 128,
        height: 128,
    }
}

pub fn run() -> eframe::Result<()> {
    // 语言在这里定一次：读取记住的选择，没有则按系统 UI 语言判断
    let lang = i18n::load();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 700.0])
            .with_min_inner_size([720.0, 420.0])
            .with_icon(app_icon())
            .with_title(window_title(lang)),
        ..Default::default()
    };
    eframe::run_native(
        "mftfind",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, lang)))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用界面同一套字体栈渲染一帧，报告某个字符有没有字形。
    fn renders(text: &str) -> bool {
        let ctx = egui::Context::default();
        install_fonts(&ctx);
        let font = egui::FontId::proportional(14.0);
        let mut ok = false;
        // eframe 0.36 里 egui 的入口是 run_ui（接收 &mut Ui），不再是 run
        let mut out = ctx.run_ui(Default::default(), |ui| {
            ok = ui.ctx().fonts_mut(|f| f.has_glyphs(&font, text));
        });
        // epaint 要求渲染产生的纹理增量被处理掉，否则析构时会 panic
        out.textures_delta.clear();
        ok
    }

    /// 菜单里的勾必须真能渲染出来。
    ///
    /// 这里踩过一次坑：egui 0.36 自带的三个字体（Ubuntu-Light / NotoEmoji /
    /// emoji-icon-font）**都不含** `✓`，界面上显示成豆腐块空框——用户就是这么
    /// 发现的。实测它们的符号覆盖只有 `☐ ● • · √ ★ ☆ → ⛃ ☜`，
    /// `✓ ✔ ☑ ☀` 全缺；补上 Segoe UI Symbol 之后 `✓` 才有。
    /// 所以这个测试同时锁住两件事：勾的字形、以及提供它的那个回退字体。
    #[test]
    fn menu_check_mark_is_actually_renderable() {
        assert!(
            renders(CHECK_MARK.trim()),
            "字体栈里没有 {:?} 的字形，菜单会显示成豆腐块——检查 install_fonts 里的符号字体回退",
            CHECK_MARK.trim()
        );
        // 顺带确认 CJK 字体那一路回退也还在（中文界面全靠它）
        assert!(renders("中"), "CJK 字体回退失效，中文会显示成豆腐块");
    }

    /// 时间戳的「未知」约定：<= 0 一律不显示。
    /// MFT 枚举路径拿不到时间戳，落盘时写 0；旧索引里还有 -11644473600
    /// （FILETIME 0 减偏移的产物），两种都得当未知处理，否则会显示成 1601 年。
    #[test]
    fn non_positive_timestamps_are_unknown() {
        assert!(fmt_time(0).is_empty());
        assert!(fmt_time(-11_644_473_600).is_empty());
        assert!(fmt_time(-1).is_empty());
        assert!(!fmt_time(1_700_000_000).is_empty());
        assert_eq!(fmt_time(1_700_000_000).len(), 16); // YYYY/MM/DD HH:MM
    }

    /// 索引里缺的大小与时间能不能从文件系统补上——MFT 路径靠的就是这条兜底。
    #[test]
    fn missing_fields_are_filled_from_disk() {
        let dir = std::env::temp_dir().join(format!("mftfind-fill-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("sample.bin");
        std::fs::write(&file, vec![0u8; 2048]).unwrap();

        let (size, time) = meta_from_disk(&file.to_string_lossy(), false).expect("文件应当能读到");
        assert_eq!(size.as_deref(), Some("2.0 KB"), "大小应按索引外的真实值补齐");
        let time = time.expect("文件系统能给出修改时间");
        assert_eq!(time.len(), 16, "时间格式应为 YYYY/MM/DD HH:MM，实际 {time:?}");
        assert_eq!(time.as_bytes()[4], b'/');

        // 目录：大小留空（显示为 "-"），时间照样要有
        let (size, time) = meta_from_disk(&dir.to_string_lossy(), true).expect("目录应当能读到");
        assert!(size.is_none(), "目录不应报出字节数");
        assert!(time.is_some(), "目录也有修改时间");

        // 不存在的路径：整条失败，调用方保持 "-"
        let missing = dir.join("no-such-file");
        assert!(meta_from_disk(&missing.to_string_lossy(), false).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
