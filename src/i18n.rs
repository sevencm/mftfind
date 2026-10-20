//! 界面多语言（i18n）。
//!
//! 设计原则：**每支持一种语言 = 加一张 `Strings` 常量表 + 在 `Lang` 里加一个变体**。
//! 界面代码只引用 `Strings` 的字段，不写任何界面字面量，也不需要在调用处 match 语言。
//! 带参数的文案统一用 `{}` 占位，经 `fill` 填充，这样两种语言的语序差异
//! （中文「命中 3 条」vs 英文「3 hits」）不会渗进调用点。

use std::fmt::{Display, Write as _};
use std::path::PathBuf;

/// 支持的语言。加语言时在这里加变体、在 `strings`/`endonym`/`code`/`from_code`
/// 各补一条分支、并新增一张 `Strings` 常量表，其余代码不用动。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Zh,
    En,
}

impl Lang {
    pub const ALL: [Lang; 2] = [Lang::Zh, Lang::En];

    pub const fn strings(self) -> &'static Strings {
        match self {
            Lang::Zh => &ZH,
            Lang::En => &EN,
        }
    }

    /// 语言用自身的语言书写，语言选择器里显示这个（中文 / English），
    /// 而不是「Chinese」——看不懂当前界面语言的人也能找到自己的那一项。
    pub const fn endonym(self) -> &'static str {
        match self {
            Lang::Zh => "中文",
            Lang::En => "English",
        }
    }

    /// 落盘用的短码
    const fn code(self) -> &'static str {
        match self {
            Lang::Zh => "zh",
            Lang::En => "en",
        }
    }

    fn from_code(s: &str) -> Option<Lang> {
        match s.trim().to_ascii_lowercase().as_str() {
            "zh" | "cn" | "zh-cn" | "zh-hans" | "zh-sg" => Some(Lang::Zh),
            "en" | "en-us" | "en-gb" => Some(Lang::En),
            _ => None,
        }
    }
}

/// 界面全部文案。字段按界面区域分组。
pub struct Strings {
    // ---------- 顶部搜索栏 ----------
    pub search_label: &'static str,
    pub search_hint: &'static str,
    pub path_mode: &'static str,
    pub sort_name: &'static str,
    pub sort_modified: &'static str,
    pub sort_size: &'static str,
    pub sort_kind: &'static str,
    pub choose_scope: &'static str,
    pub reload_indexes: &'static str,
    pub restart_admin: &'static str,
    pub language: &'static str,

    // ---------- 状态提示（无参数）----------
    pub status_elevated: &'static str,
    pub status_not_elevated: &'static str,
    pub status_reloaded: &'static str,
    pub status_copied: &'static str,

    // ---------- 索引进度 ----------
    pub indexing_start_tmpl: &'static str,
    pub indexing_progress_tmpl: &'static str,
    pub indexing_done_tmpl: &'static str,
    pub indexing_failed_tmpl: &'static str,
    pub ready_to_search: &'static str,
    pub method_mft: &'static str,
    pub method_walk: &'static str,
    pub mft_fallback_tmpl: &'static str,
    pub fallback_no_records: &'static str,

    // ---------- 底部状态栏 ----------
    pub volumes_and_records_tmpl: &'static str,
    pub hit_count_tmpl: &'static str,
    pub search_ms_tmpl: &'static str,
    pub type_to_search: &'static str,

    // ---------- 索引范围窗口 ----------
    pub scope_title: &'static str,
    pub scope_hint: &'static str,
    pub already_indexed: &'static str,
    pub start_indexing: &'static str,
    pub force_walk: &'static str,
    pub scope_elevated: &'static str,
    pub scope_not_elevated: &'static str,

    // ---------- 结果表 ----------
    pub col_name: &'static str,
    pub col_path: &'static str,
    pub col_size: &'static str,
    pub col_modified: &'static str,

    // ---------- 结果行操作（右键菜单与工具栏按钮共用同一套文案） ----------
    pub menu_open: &'static str,
    pub menu_reveal: &'static str,
    pub menu_copy: &'static str,
    /// 工具栏按钮的悬停提示，参数是键名（键名本身不翻译，只有这句话要翻译）
    pub shortcut_tip_tmpl: &'static str,
    pub open_failed_tmpl: &'static str,
    pub reveal_failed_tmpl: &'static str,

    // ---------- 窗口标题 ----------
    pub window_title: &'static str,
}

impl Strings {
    pub fn indexing_start(&self, letter: char) -> String {
        fill(self.indexing_start_tmpl, &[&letter])
    }

    pub fn indexing_progress(&self, letter: char, count: usize) -> String {
        fill(self.indexing_progress_tmpl, &[&letter, &count])
    }

    pub fn indexing_done(
        &self,
        letter: char,
        count: usize,
        scan_secs: f64,
        total_secs: f64,
        method: &str,
        fallback: &str,
    ) -> String {
        fill(
            self.indexing_done_tmpl,
            &[
                &letter,
                &count,
                &fmt_secs(scan_secs),
                &fmt_secs(total_secs),
                &method,
                &fallback,
            ],
        )
    }

    pub fn indexing_failed(&self, letter: char, err: impl Display) -> String {
        fill(self.indexing_failed_tmpl, &[&letter, &err])
    }

    pub fn mft_fallback(&self, reason: &str) -> String {
        fill(self.mft_fallback_tmpl, &[&reason])
    }

    pub fn volumes_and_records(&self, volumes: usize, records: usize) -> String {
        fill(self.volumes_and_records_tmpl, &[&volumes, &records])
    }

    pub fn hit_count(&self, n: usize) -> String {
        fill(self.hit_count_tmpl, &[&n])
    }

    pub fn search_ms(&self, ms: f64) -> String {
        fill(self.search_ms_tmpl, &[&fmt_ms(ms)])
    }

    pub fn open_failed(&self, err: impl Display) -> String {
        fill(self.open_failed_tmpl, &[&err])
    }

    pub fn reveal_failed(&self, err: impl Display) -> String {
        fill(self.reveal_failed_tmpl, &[&err])
    }

    /// 快捷键提示。参数是键名——键名各语言一致（键盘上印的就是那个），
    /// 需要翻译的只有「快捷键」这几个字。
    pub fn shortcut_tip(&self, keys: impl Display) -> String {
        fill(self.shortcut_tip_tmpl, &[&keys])
    }
}

/// 把模板里的 `{}` 依次替换为参数。占位符比参数多时保留原文的 `{}`，
/// 宁可显示得奇怪也不 panic。
fn fill(tmpl: &str, args: &[&dyn Display]) -> String {
    let mut out = String::with_capacity(tmpl.len() + 32);
    let mut parts = tmpl.split("{}");
    // 第一段是占位符之前的前缀，直接放进去；之后每段前面都要先填一个参数。
    if let Some(head) = parts.next() {
        out.push_str(head);
    }
    let mut args = args.iter();
    for part in parts {
        match args.next() {
            Some(a) => {
                let _ = write!(out, "{}", a);
            }
            None => out.push_str("{}"),
        }
        out.push_str(part);
    }
    out
}

/// 秒保留 1 位小数
fn fmt_secs(v: f64) -> String {
    format!("{:.1}", v)
}

/// 毫秒保留 1 位小数
fn fmt_ms(v: f64) -> String {
    format!("{:.1}", v)
}

// ============================================================================
// 文案表
// ============================================================================

const ZH: Strings = Strings {
    search_label: "搜索",
    search_hint: "文件名关键词，多关键词空格分隔（含 \\ 则匹配完整路径）",
    path_mode: "路径",
    sort_name: "名称",
    sort_modified: "时间",
    sort_size: "大小",
    sort_kind: "类型",
    choose_scope: "选择索引范围 / 重建",
    reload_indexes: "重新加载索引",
    restart_admin: "以管理员身份重启",
    language: "语言",

    status_elevated: "已以管理员运行，索引将走 MFT 快路径",
    status_not_elevated: "当前非管理员：MFT 不可用会回退并行遍历（点右上角按钮可提权重启）",
    status_reloaded: "已重新加载索引文件",
    status_copied: "路径已复制到剪贴板",

    indexing_start_tmpl: "正在索引 {}: …",
    indexing_progress_tmpl: "正在索引 {}: 已 {} 条…",
    indexing_done_tmpl: "{}: 完成 {} 条，扫描 {}s，总计 {}s（{}{}）",
    indexing_failed_tmpl: "{}: 索引失败 - {}",
    ready_to_search: "（可开始搜索）",
    method_mft: "MFT",
    method_walk: "并行遍历",
    mft_fallback_tmpl: "，MFT 回退原因：{}",
    fallback_no_records: "MFT 未返回记录",

    volumes_and_records_tmpl: "索引 {} 个卷 / {} 条",
    hit_count_tmpl: "命中 {} 条",
    search_ms_tmpl: "搜索 {} ms",
    type_to_search: "输入关键词开始搜索",

    scope_title: "选择索引范围",
    scope_hint: "勾选需要索引的磁盘（管理员权限下走 MFT，约 1~3 秒/百万文件）：",
    already_indexed: "（已有索引）",
    start_indexing: "开始索引",
    force_walk: "强制用遍历（不提权）",
    scope_elevated: "当前已是管理员：将优先使用 MFT 快路径",
    scope_not_elevated: "当前非管理员：MFT 会被拒绝，自动回退并行遍历",

    col_name: "名称",
    col_path: "路径",
    col_size: "大小",
    col_modified: "修改时间",

    menu_open: "打开",
    menu_reveal: "打开所在目录",
    menu_copy: "复制完整路径",
    shortcut_tip_tmpl: "快捷键 {}",
    open_failed_tmpl: "打开失败：{}",
    reveal_failed_tmpl: "打开目录失败：{}",

    window_title: "极速文件搜索",
};

const EN: Strings = Strings {
    search_label: "Search",
    search_hint: "Filename keywords, space separated (include \\ to match the whole path)",
    path_mode: "Path",
    sort_name: "Name",
    sort_modified: "Modified",
    sort_size: "Size",
    sort_kind: "Type",
    choose_scope: "Choose volumes / rebuild",
    reload_indexes: "Reload indexes",
    restart_admin: "Restart as administrator",
    language: "Language",

    status_elevated: "Running elevated — indexing will use the MFT fast path",
    status_not_elevated: "Not elevated: MFT is unavailable, indexing falls back to a parallel walk (restart elevated with the button above)",
    status_reloaded: "Index files reloaded",
    status_copied: "Path copied to clipboard",

    indexing_start_tmpl: "Indexing {}: …",
    indexing_progress_tmpl: "Indexing {}: {} records…",
    indexing_done_tmpl: "{}: {} records, scan {}s, total {}s ({}{})",
    indexing_failed_tmpl: "{}: indexing failed - {}",
    ready_to_search: " (ready to search)",
    method_mft: "MFT",
    method_walk: "parallel walk",
    mft_fallback_tmpl: ", MFT fallback: {}",
    fallback_no_records: "MFT returned no records",

    volumes_and_records_tmpl: "{} volumes / {} records",
    hit_count_tmpl: "{} hits",
    search_ms_tmpl: "search {} ms",
    type_to_search: "Type a keyword to search",

    scope_title: "Choose index scope",
    scope_hint: "Select the volumes to index (MFT is used when elevated, ~1-3 s per million files):",
    already_indexed: " (indexed)",
    start_indexing: "Start indexing",
    force_walk: "Force directory walk (no elevation)",
    scope_elevated: "Already elevated: the MFT fast path will be used",
    scope_not_elevated: "Not elevated: MFT will be denied, falling back to a parallel walk",

    col_name: "Name",
    col_path: "Path",
    col_size: "Size",
    col_modified: "Modified",

    menu_open: "Open",
    menu_reveal: "Open containing folder",
    menu_copy: "Copy full path",
    shortcut_tip_tmpl: "Shortcut {}",
    open_failed_tmpl: "Could not open: {}",
    reveal_failed_tmpl: "Could not show in Explorer: {}",

    window_title: "Instant file search",
};

// ============================================================================
// 语言的选择与记忆
// ============================================================================

/// 首次启动（或配置损坏）时按系统 UI 语言决定，其余语言一律回落到英文。
fn system_lang() -> Lang {
    match crate::win::ui_language() {
        // LANGID 低 10 位是主语言 ID，0x04 = LANG_CHINESE
        Some(id) if id & 0x03FF == 0x04 => Lang::Zh,
        _ => Lang::En,
    }
}

/// 配置文件：`%APPDATA%\mftfind\ui.ini`。
///
/// 放在 APPDATA 而不是 exe 同级，是因为 exe 可能落在 Program Files 这类
/// 只读目录，那时写盘会失败、设置就静默丢失；APPDATA 对普通用户永远可写。
fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")
        .or_else(|| std::env::var_os("LOCALAPPDATA"))
        .map(PathBuf::from)?;
    Some(base.join("mftfind").join("ui.ini"))
}

/// 读取记住的语言，没有就按系统语言判断。
pub fn load() -> Lang {
    if let Some(p) = config_path() {
        if let Ok(text) = std::fs::read_to_string(p) {
            if let Some(lang) = parse(&text) {
                return lang;
            }
        }
    }
    system_lang()
}

/// 记住用户的选择。写失败不影响使用，只是下次启动回到系统语言。
pub fn save(lang: Lang) {
    let Some(path) = config_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // 只有一行，不引入任何 ini/toml 解析依赖
    let _ = std::fs::write(path, format!("lang={}\n", lang.code()));
}

fn parse(text: &str) -> Option<Lang> {
    text.lines()
        .find_map(|line| line.strip_prefix("lang="))
        .and_then(Lang::from_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 所有带参数的模板配对。
    /// 中英两版的占位符个数必须一致——少一个 `{}` 不会 panic，
    /// 只会让界面静默少显示一段信息（例如丢掉「扫描 1.5s」），很难靠肉眼发现。
    fn template_pairs() -> [(&'static str, &'static str); 11] {
        [
            (ZH.indexing_start_tmpl, EN.indexing_start_tmpl),
            (ZH.indexing_progress_tmpl, EN.indexing_progress_tmpl),
            (ZH.indexing_done_tmpl, EN.indexing_done_tmpl),
            (ZH.indexing_failed_tmpl, EN.indexing_failed_tmpl),
            (ZH.mft_fallback_tmpl, EN.mft_fallback_tmpl),
            (ZH.volumes_and_records_tmpl, EN.volumes_and_records_tmpl),
            (ZH.hit_count_tmpl, EN.hit_count_tmpl),
            (ZH.search_ms_tmpl, EN.search_ms_tmpl),
            (ZH.shortcut_tip_tmpl, EN.shortcut_tip_tmpl),
            (ZH.open_failed_tmpl, EN.open_failed_tmpl),
            (ZH.reveal_failed_tmpl, EN.reveal_failed_tmpl),
        ]
    }

    fn placeholders(t: &str) -> usize {
        t.matches("{}").count()
    }

    #[test]
    fn fill_replaces_in_order() {
        assert_eq!(fill("a {} b {} c", &[&1, &2]), "a 1 b 2 c");
        assert_eq!(fill("no placeholders", &[]), "no placeholders");
        assert_eq!(fill("{}", &[&"x"]), "x");
        assert_eq!(fill("{}尾", &[&"头"]), "头尾");
    }

    #[test]
    fn fill_keeps_placeholder_when_args_missing() {
        assert_eq!(fill("{} {}", &[&1]), "1 {}");
    }

    #[test]
    fn zh_and_en_agree_on_placeholder_counts() {
        for (zh, en) in template_pairs() {
            assert_eq!(
                placeholders(zh),
                placeholders(en),
                "中英模板占位符个数不一致: zh={zh:?} en={en:?}"
            );
        }
    }

    #[test]
    fn parameterized_messages_substitute_everything() {
        for lang in Lang::ALL {
            let s = lang.strings();
            let samples = [
                s.indexing_start('C'),
                s.indexing_progress('C', 1234),
                s.indexing_done('C', 1234, 1.5, 2.5, s.method_mft, ""),
                s.indexing_done('D', 9, 0.1, 0.2, s.method_walk, &s.mft_fallback("denied")),
                s.indexing_failed('C', "access denied"),
                s.mft_fallback("access denied"),
                s.volumes_and_records(7, 9_214_651),
                s.hit_count(42),
                s.search_ms(150.0),
                s.open_failed("err"),
                s.reveal_failed("err"),
                s.shortcut_tip("Ctrl+Enter"),
            ];
            for text in samples {
                assert!(!text.contains("{}"), "存在未替换的占位符: {text:?}");
            }
        }
    }

    #[test]
    fn numeric_formatting_keeps_one_decimal() {
        assert!(ZH.search_ms(150.0).contains("150.0"));
        // 用二进制可精确表示的数，避免依赖浮点的舍入模式
        assert!(EN.indexing_done('C', 1, 1.5, 2.5, "MFT", "").contains("1.5s"));
        assert!(EN.indexing_done('C', 1, 1.5, 2.5, "MFT", "").contains("2.5s"));
    }

    #[test]
    fn config_code_round_trip() {
        for lang in Lang::ALL {
            assert_eq!(Lang::from_code(lang.code()), Some(lang));
            assert_eq!(parse(&format!("lang={}\n", lang.code())), Some(lang));
        }
        assert_eq!(Lang::from_code("ZH-CN"), Some(Lang::Zh));
        assert_eq!(parse(""), None);
        assert_eq!(parse("lang=fr"), None);
        assert_eq!(parse("other=1"), None);
    }

    #[test]
    fn every_language_is_usable() {
        for lang in Lang::ALL {
            assert!(!lang.endonym().is_empty());
            assert!(!lang.strings().window_title.is_empty());
            // 语言选择器显示的名字必须是该语言自身书写的
            assert!(!lang.strings().language.is_empty());
        }
    }
}
