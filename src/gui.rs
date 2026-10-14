//! egui 界面：顶部搜索框 + 虚拟滚动结果表 + 右键菜单 + 索引导航。
//!
//! 交互要点：
//! - 输入即搜：查询变化后丢给后台线程，避免阻塞界面（结果通过 Arc<Mutex<>> 回传）
//! - 结果表用 ScrollArea::show_rows 做虚拟滚动，百万级结果也不卡
//! - MFT 索引不含文件大小，只对「当前可见行」惰性调用 metadata 补齐并缓存

use std::collections::HashMap;
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

/// 搜索框右侧控件（路径勾选框 + 排序下拉）预留的宽度。
/// 固定值避免切语言时搜索框跳动；语言选择在菜单栏，不占这行宽度。
const CONTROLS_WIDTH: f32 = 230.0;

struct Meta {
    ms: f64,
    total: usize,
    generation: u64,
    path_ready_ms: f64,
}

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
    size_cache: HashMap<(usize, u32), String>,
    selected: Option<usize>,
    status: Status,
    index_status: Arc<Mutex<IndexStatus>>,
    indexing: bool,
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
        install_cjk_font(&cc.egui_ctx);
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
            size_cache: HashMap::new(),
            selected: None,
            status: if elevated {
                Status::Elevated
            } else {
                Status::NotElevated
            },
            index_status: Arc::new(Mutex::new(IndexStatus::None)),
            indexing: false,
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
        self.size_cache.clear();
        self.drives = list_drive_items();
    }

    fn start_indexing(&mut self, letters: Vec<char>, ctx: &egui::Context, force_walk: bool) {
        if self.indexing || letters.is_empty() {
            return;
        }
        self.indexing = true;
        let status = self.index_status.clone();
        let ctx = ctx.clone();
        let elevated = self.elevated;
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
            // 全部卷结束，在最终状态上标记「可开始搜索」
            if let IndexStatus::Done(d) = &mut *status.lock().unwrap() {
                d.ready = true;
            }
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

/// 加载中文字体（egui 自带字体不含 CJK）。
///
/// 注意是 **push 到末尾** 而不是插到开头：egui 按家族列表顺序逐字查找字形，
/// 放在末尾意味着拉丁字母仍用 egui 自带字体渲染（更好看），只有缺字时才回退到中文字体。
/// 放到开头会让英文界面里所有字母都用黑体渲染。
fn install_cjk_font(ctx: &egui::Context) {
    let candidates = [
        "C:/Windows/Fonts/simhei.ttf",
        "C:/Windows/Fonts/Deng.ttf",
        "C:/Windows/Fonts/msyh.ttc",
        "C:/Windows/Fonts/simsun.ttc",
    ];
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert(
                "cjk".to_owned(),
                Arc::new(egui::FontData::from_owned(bytes)),
            );
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .push("cjk".to_owned());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push("cjk".to_owned());
            ctx.set_fonts(fonts);
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

        // ---------- 菜单栏 ----------
        egui::Panel::top("menu").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                let before = self.lang;
                ui.menu_button(s.language, |ui| {
                    for lang in Lang::ALL {
                        // 当前语言打勾；选项文字用各自语言书写，看不懂当前界面语言
                        // 的人也能在菜单里认出自己的那一项
                        let mark = if lang == self.lang { "✓ " } else { "   " };
                        ui.selectable_value(&mut self.lang, lang, format!("{mark}{}", lang.endonym()));
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

                        // 惰性补齐文件大小（MFT 索引里没有尺寸）
                        let size_text = match ix.size(h.idx) {
                            Some(s) if s != u64::MAX => human_size(s),
                            _ => {
                                if let Some(cached) = self.size_cache.get(&(h.vol, h.idx)) {
                                    cached.clone()
                                } else {
                                    let t = std::fs::metadata(&full)
                                        .map(|m| human_size(m.len()))
                                        .unwrap_or_else(|_| "-".to_string());
                                    self.size_cache.insert((h.vol, h.idx), t.clone());
                                    t
                                }
                            }
                        };

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
                                ui.add_sized([w_time, row_h], egui::Label::new(fmt_time(ix.mtime(h.idx))));
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

pub fn run() -> eframe::Result<()> {
    // 语言在这里定一次：读取记住的选择，没有则按系统 UI 语言判断
    let lang = i18n::load();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 700.0])
            .with_min_inner_size([720.0, 420.0])
            .with_title(window_title(lang)),
        ..Default::default()
    };
    eframe::run_native(
        "mftfind",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, lang)))),
    )
}
