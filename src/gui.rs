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
use crate::search::{parse_query, search, search_paths, sort_hits_multi, Hit, SortBy};
use crate::win;

/// 单次搜索最多保留的命中数（界面只显示可见行）
const MAX_HITS: usize = 20_000;
const ROW_HEIGHT: f32 = 20.0;

struct Meta {
    ms: f64,
    total: usize,
    generation: u64,
    path_ready_ms: f64,
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
    status: String,
    index_status: Arc<Mutex<String>>,
    indexing: bool,
    drives: Vec<DriveItem>,
    show_scope: bool,
    elevated: bool,
    total_records: usize,
    focus_search: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> App {
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
                "已以管理员运行，索引将走 MFT 快路径".to_string()
            } else {
                "当前非管理员：MFT 不可用会回退并行遍历（点右上角按钮可提权重启）".to_string()
            },
            index_status: Arc::new(Mutex::new(String::new())),
            indexing: false,
            drives: list_drive_items(),
            show_scope: false,
            elevated,
            total_records,
            focus_search: true,
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
                *status.lock().unwrap() = format!("正在索引 {}: …", letter);
                let mut last = 0usize;
                let res = build::build_volume(letter, force_walk && !elevated, &mut |n| {
                    if n - last >= 100_000 {
                        last = n;
                        *status.lock().unwrap() =
                            format!("正在索引 {}: 已 {} 条…", letter, n);
                    }
                });
                match res {
                    Ok(o) => {
                        *status.lock().unwrap() = format!(
                            "{}: 完成 {} 条，扫描 {:.1}s，总计 {:.1}s（{}{}）",
                            letter,
                            o.count,
                            o.scan_secs,
                            o.total_secs,
                            if o.used_mft { "MFT" } else { "并行遍历" },
                            match &o.fallback_reason {
                                Some(r) => format!("，MFT 回退原因：{}", r),
                                None => String::new(),
                            }
                        );
                    }
                    Err(e) => {
                        *status.lock().unwrap() = format!("{}: 索引失败 - {}", letter, e);
                    }
                }
                ctx.request_repaint();
            }
            *status.lock().unwrap() = format!("{}（可开始搜索）", status.lock().unwrap());
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

/// 加载中文字体（egui 自带字体不含 CJK）
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
                .insert(0, "cjk".to_owned());
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

        // ---------- 快捷键 ----------
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.query.clear();
        }
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::F)) {
            self.focus_search = true;
        }

        // ---------- 顶部搜索栏 ----------
        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("搜索");
                let desired = ui.available_width()
                    - if self.elevated { 300.0 } else { 300.0 };
                let resp = ui.add_sized(
                    [desired.max(200.0), 24.0],
                    egui::TextEdit::singleline(&mut self.query).hint_text("文件名关键词，多关键词空格分隔（含 \\ 则匹配完整路径）"),
                );
                if self.focus_search {
                    resp.request_focus();
                    self.focus_search = false;
                }
                ui.checkbox(&mut self.path_mode, "路径");
                egui::ComboBox::from_id_salt("sort")
                    .selected_text(match self.sort {
                        SortBy::Name => "名称",
                        SortBy::Modified => "时间",
                        SortBy::Size => "大小",
                        SortBy::Kind => "类型",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.sort, SortBy::Name, "名称");
                        ui.selectable_value(&mut self.sort, SortBy::Modified, "时间");
                        ui.selectable_value(&mut self.sort, SortBy::Size, "大小");
                        ui.selectable_value(&mut self.sort, SortBy::Kind, "类型");
                    });
            });
            ui.horizontal(|ui| {
                if ui.button("选择索引范围 / 重建").clicked() {
                    self.drives = list_drive_items();
                    self.show_scope = true;
                }
                if ui.button("重新加载索引").clicked() {
                    self.reload_indexes();
                    self.status = "已重新加载索引文件".to_string();
                }
                if !self.elevated && ui.button("以管理员身份重启").clicked() {
                    let _ = win::restart_as_admin("");
                }
                let s = self.index_status.lock().unwrap().clone();
                if !s.is_empty() {
                    ui.label(egui::RichText::new(s).color(egui::Color32::from_rgb(200, 150, 60)));
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
                ui.label(format!(
                    "索引 {} 个卷 / {} 条",
                    self.indexes.len(),
                    self.total_records
                ));
                ui.separator();
                if meta.total > 0 {
                    ui.label(format!("命中 {} 条", meta.total));
                    ui.label(format!("搜索 {:.1} ms", meta.ms));
                } else {
                    ui.label("输入关键词开始搜索");
                }
                ui.separator();
                ui.label(&self.status);
            });
        });

        // ---------- 索引范围窗口 ----------
        if self.show_scope {
            let mut open = self.show_scope;
            let mut start = false;
            let mut force_walk = false;
            egui::Window::new("选择索引范围")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .show(&ctx, |ui| {
                    ui.label("勾选需要索引的磁盘（管理员权限下走 MFT，约 1~3 秒/百万文件）：");
                    for d in self.drives.iter_mut() {
                        ui.horizontal(|ui| {
                            ui.checkbox(
                                &mut d.checked,
                                format!(
                                    "{}:\\  {} [{}] {}",
                                    d.letter,
                                    d.label,
                                    d.fs,
                                    if d.indexed { "（已有索引）" } else { "" }
                                ),
                            );
                        });
                    }
                    ui.separator();
                    ui.horizontal(|ui| {
                        if ui.button("开始索引").clicked() {
                            start = true;
                        }
                        ui.checkbox(&mut force_walk, "强制用遍历（不提权）");
                    });
                    ui.label(
                        egui::RichText::new(if self.elevated {
                            "当前已是管理员：将优先使用 MFT 快路径"
                        } else {
                            "当前非管理员：MFT 会被拒绝，自动回退并行遍历"
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
                ui.add_sized([w_name, row_h], egui::Label::new(egui::RichText::new("名称").strong()));
                ui.add_sized([w_path, row_h], egui::Label::new(egui::RichText::new("路径").strong()));
                ui.add_sized([w_size, row_h], egui::Label::new(egui::RichText::new("大小").strong()));
                ui.add_sized([w_time, row_h], egui::Label::new(egui::RichText::new("修改时间").strong()));
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
                            if ui.button("打开").clicked() {
                                open_path = Some((full.clone(), is_dir));
                            }
                            if ui.button("打开所在文件夹").clicked() {
                                reveal = Some(full.clone());
                            }
                            if ui.button("复制完整路径").clicked() {
                                copy = Some(full.clone());
                            }
                        });
                    }
                });

            if let Some((p, d)) = open_path {
                if let Err(e) = win::open_path(&p, d) {
                    self.status = format!("打开失败：{}", e);
                }
            }
            if let Some(p) = reveal {
                if let Err(e) = win::reveal_in_explorer(&p) {
                    self.status = format!("打开目录失败：{}", e);
                }
            }
            if let Some(p) = copy {
                ctx.copy_text(p);
                self.status = "路径已复制到剪贴板".to_string();
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

pub fn run() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 700.0])
            .with_min_inner_size([720.0, 420.0])
            .with_title("mftfind - 极速文件搜索"),
        ..Default::default()
    };
    eframe::run_native(
        "mftfind",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}
