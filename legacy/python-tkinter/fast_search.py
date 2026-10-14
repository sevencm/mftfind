# -*- coding: utf-8 -*-
"""
FastSearch —— 仿 Everything 的 Windows 快速文件搜索工具

原理说明：
  Everything 直接读取 NTFS 的 MFT（需要管理员权限），所以能做到毫秒级索引。
  本工具采用「后台遍历 + 索引缓存 + 输入即搜」的方案，不需要管理员权限：
  1. 首次启动让你选择要索引的磁盘（默认仅 C 盘），后台线程用 os.scandir 遍历；
  2. 逐个磁盘建索引，每完成一个磁盘立即落盘缓存，中途关窗也不丢进度；
  3. 下次启动直接加载缓存（旧缓存先加载、再后台刷新），秒级可用；
  4. 搜索时在内存中对文件名做子串/多关键词匹配，输入即时出结果。

用法：  python fast_search.py
依赖：  仅 Python 标准库（Windows 下 tkinter 自带）
"""

import os
import sys
import time
import pickle
import string
import threading
import subprocess
import ctypes

import tkinter as tk
from tkinter import ttk, messagebox

# ---------------------------------------------------------------- 配置

APP_TITLE = "FastSearch - 文件快速搜索"
APP_DIR = os.path.dirname(os.path.abspath(__file__))
CACHE_FILE = os.path.join(APP_DIR, ".index_cache.pkl")
CONFIG_FILE = os.path.join(APP_DIR, ".config.pkl")
PAGE_SIZE = 500            # 每次向列表追加的行数（虚拟滚动）
CACHE_MAX_AGE = 24 * 3600  # 缓存新鲜度阈值：超过则启动时后台自动刷新

# 遍历时跳过的目录（整棵子树不参与索引），可按需增删
SKIP_DIRS = {
    "$recycle.bin", "system volume information", "$getcurrent",
    "windowsapps", "wuds", "appxpatched", "$windows.~ws",
}
# Windows 重解析点（junction / 挂载点），跳过以免重复遍历或死循环
FILE_ATTRIBUTE_REPARSE_POINT = 0x400

# ---------------------------------------------------------------- 索引构建


def list_available_drives():
    """枚举所有固定磁盘盘符（含卷标，便于选择）。"""
    drives = []
    bitmask = ctypes.windll.kernel32.GetLogicalDrives()
    get_vol = ctypes.windll.kernel32.GetVolumeInformationW
    get_vol.argtypes = [ctypes.c_wchar_p, ctypes.c_wchar_p, ctypes.c_uint32,
                        ctypes.POINTER(ctypes.c_uint32), ctypes.POINTER(ctypes.c_uint32),
                        ctypes.POINTER(ctypes.c_uint32), ctypes.c_wchar_p, ctypes.c_uint32]
    get_vol.restype = ctypes.c_int
    for i, letter in enumerate(string.ascii_uppercase):
        if bitmask & (1 << i):
            drive = f"{letter}:\\"
            if not os.path.isdir(drive):
                continue
            if ctypes.windll.kernel32.GetDriveTypeW(drive) != 3:  # 仅 DRIVE_FIXED
                continue
            label, fs_name = "", ""
            name_buf = ctypes.create_unicode_buffer(261)
            fs_buf = ctypes.create_unicode_buffer(261)
            vol_serial = ctypes.c_uint32()
            max_len = ctypes.c_uint32()
            flags = ctypes.c_uint32()
            try:
                if get_vol(drive, name_buf, 261, ctypes.byref(vol_serial),
                           ctypes.byref(max_len), ctypes.byref(flags),
                           fs_buf, 261):
                    label, fs_name = name_buf.value, fs_buf.value
            except Exception:
                pass
            drives.append((drive, label, fs_name))
    if not drives:
        drives = [("C:\\", "", "")]
    return drives


def list_fixed_drives():
    """兼容旧调用：返回所有固定磁盘路径。"""
    return [d for d, _, _ in list_available_drives()]


def load_config():
    try:
        if os.path.exists(CONFIG_FILE):
            with open(CONFIG_FILE, "rb") as f:
                return pickle.load(f)
    except (OSError, EOFError, pickle.UnpicklingError):
        pass
    return {}


def save_config(cfg):
    try:
        with open(CONFIG_FILE, "wb") as f:
            pickle.dump(cfg, f, protocol=pickle.HIGHEST_PROTOCOL)
    except OSError:
        pass


def build_index(roots, progress_cb=None, cancel_event=None):
    """
    遍历 roots 下所有文件与目录，返回索引列表。
    每条记录: (name_lower, name, path, size, mtime, is_dir)
    """
    index = []
    scan_stack = list(roots)
    last_report = 0
    while scan_stack:
        if cancel_event is not None and cancel_event.is_set():
            return index
        current = scan_stack.pop()
        try:
            with os.scandir(current) as it:
                for entry in it:
                    try:
                        if entry.is_symlink():
                            continue
                        is_dir = entry.is_dir(follow_symlinks=False)
                        try:
                            stat = entry.stat(follow_symlinks=False)
                        except OSError:
                            stat = None
                        if is_dir:
                            # 跳过重解析点，避免重复遍历
                            if stat is not None and (
                                    getattr(stat, "st_file_attributes", 0)
                                    & FILE_ATTRIBUTE_REPARSE_POINT):
                                continue
                            if entry.name.lower() in SKIP_DIRS:
                                continue
                            size, mtime = 0, (stat.st_mtime if stat else 0.0)
                        else:
                            size = stat.st_size if stat else 0
                            mtime = stat.st_mtime if stat else 0.0
                        path = entry.path
                        index.append((entry.name.lower(), entry.name, path, size, mtime, is_dir))
                        if is_dir:
                            scan_stack.append(path)
                    except OSError:
                        continue
        except (PermissionError, OSError):
            continue
        if progress_cb and len(index) - last_report >= 20000:
            last_report = len(index)
            progress_cb(len(index))
    if progress_cb:
        progress_cb(len(index))
    return index


def save_cache(index, build_time):
    """原子写入缓存，避免中途关窗留下损坏文件。"""
    tmp = CACHE_FILE + ".tmp"
    try:
        with open(tmp, "wb") as f:
            pickle.dump({"time": build_time, "index": index}, f, protocol=pickle.HIGHEST_PROTOCOL)
        os.replace(tmp, CACHE_FILE)
    except OSError:
        pass


def load_cache():
    """返回 (index, build_time)，任何年龄的缓存都返回（由调用方决定是否后台刷新）。"""
    try:
        if os.path.exists(CACHE_FILE):
            with open(CACHE_FILE, "rb") as f:
                data = pickle.load(f)
            return data["index"], data["time"]
    except (OSError, EOFError, pickle.UnpicklingError):
        pass
    return None, 0.0

# ---------------------------------------------------------------- 工具函数


def fmt_size(n):
    if n == 0:
        return ""
    for unit in ("B", "KB", "MB", "GB", "TB"):
        if n < 1024:
            return f"{n:.0f} {unit}" if unit == "B" else f"{n:.1f} {unit}"
        n /= 1024
    return f"{n:.1f} PB"


def fmt_mtime(ts):
    if ts <= 0:
        return ""
    return time.strftime("%Y/%m/%d %H:%M", time.localtime(ts))


def match(query_tokens, name_lower, path_lower):
    """多关键词 AND 匹配：所有 token 都出现在文件名或路径中即命中。"""
    for t in query_tokens:
        if t not in name_lower and t not in path_lower:
            return False
    return True

# ---------------------------------------------------------------- GUI


class FastSearchApp:
    def __init__(self, root):
        self.root = root
        root.title(APP_TITLE)
        root.geometry("980x600")
        root.minsize(640, 380)

        self.index = []              # 全量索引
        self.results = []            # 当前搜索结果
        self.shown = 0               # 已显示的行数
        self.indexing = False
        self.cancel_event = threading.Event()
        self.sort_state = {"col": None, "reverse": False}
        self.config = load_config()
        self.scanned_roots = self.config.get("roots", [])

        self._build_menu()
        self._build_widgets()
        self._bind_events()
        self.root.protocol("WM_DELETE_WINDOW", self._on_close)

        # 启动：先加载缓存（无论新旧，先让工具可用），过旧或无缓存则后台建索引
        cached, build_time = load_cache()
        if cached:
            self.index = cached
            age_h = (time.time() - build_time) / 3600
            if age_h <= CACHE_MAX_AGE:
                self.status_var.set(
                    f"已加载索引缓存：{len(self.index):,} 项（{age_h:.1f} 小时前建立，"
                    f"工具-重新索引 可刷新）")
            else:
                self.status_var.set(
                    f"已加载旧索引缓存：{len(self.index):,} 项（{age_h / 24:.1f} 天前），"
                    f"正在后台刷新…")
                self.root.after(300, self.reindex)
        else:
            # 首次运行：让用户选择索引范围，默认仅 C 盘，避免一上来就全盘慢扫
            self.root.after(200, self._first_run)

    def _first_run(self):
        if self._choose_scope(first_run=True):
            self.reindex()

    def _choose_scope(self, first_run=False):
        """索引范围选择对话框，返回是否确认。"""
        drives = list_available_drives()
        current = {os.path.normcase(r) for r in self.scanned_roots}

        dlg = tk.Toplevel(self.root)
        dlg.title("选择索引范围")
        dlg.transient(self.root)
        dlg.grab_set()
        dlg.resizable(False, False)

        ttk.Label(dlg, text="勾选要建立索引的磁盘（首次建议先只选 C 盘）：",
                  padding=(12, 12, 12, 6)).pack(anchor="w")
        hint = ("· 索引只需建立一次，结果会缓存 24 小时\n"
                "· 磁盘越多、文件越多，首次索引耗时越长（可在菜单中取消）\n"
                "· 之后可随时通过 工具-选择索引范围 调整")
        ttk.Label(dlg, text=hint, foreground="gray", justify="left",
                  padding=(12, 0, 12, 8)).pack(anchor="w")

        vars_map = {}
        box = ttk.Frame(dlg, padding=(12, 0, 12, 8))
        box.pack(fill="x")
        for drive, label, fs_name in drives:
            default = os.path.normcase(drive) in current
            if first_run and not current:
                default = drive.upper().startswith("C:")
            v = tk.BooleanVar(value=default)
            text = drive
            if label:
                text += f"  {label}"
            if fs_name:
                text += f"  [{fs_name}]"
            ttk.Checkbutton(box, text=text, variable=v).pack(anchor="w")
            vars_map[drive] = v

        btns = ttk.Frame(dlg, padding=(12, 4, 12, 12))
        btns.pack(fill="x")
        confirmed = {"ok": False}

        def on_ok():
            roots = [d for d, v in vars_map.items() if v.get()]
            if not roots:
                messagebox.showwarning("提示", "请至少选择一个磁盘", parent=dlg)
                return
            self.scanned_roots = roots
            self.config["roots"] = roots
            save_config(self.config)
            confirmed["ok"] = True
            dlg.destroy()

        ttk.Button(btns, text="开始索引", command=on_ok).pack(side="right", padx=(6, 0))
        if first_run:
            def on_later():
                self.status_var.set("已跳过索引。菜单 工具-选择索引范围 可开始建立索引")
                dlg.destroy()
            ttk.Button(btns, text="以后再说", command=on_later).pack(side="right")
        else:
            ttk.Button(btns, text="取消", command=dlg.destroy).pack(side="right")
        dlg.wait_window()
        return confirmed["ok"]

    # ---------- 界面搭建 ----------

    def _build_menu(self):
        menubar = tk.Menu(self.root)

        m_file = tk.Menu(menubar, tearoff=0)
        m_file.add_command(label="退出", command=self._on_close)
        menubar.add_cascade(label="文件", menu=m_file)

        m_view = tk.Menu(menubar, tearoff=0)
        m_view.add_command(label="清空搜索框", command=self._clear_search)
        menubar.add_cascade(label="视图", menu=m_view)

        m_search = tk.Menu(menubar, tearoff=0)
        m_search.add_command(label="重新索引（刷新）", command=self.reindex)
        m_search.add_command(label="取消索引", command=self._cancel_index)
        menubar.add_cascade(label="搜索", menu=m_search)

        m_tool = tk.Menu(menubar, tearoff=0)
        m_tool.add_command(label="重新索引（刷新）", command=self.reindex)
        m_tool.add_command(label="选择索引范围…", command=self._change_scope)
        m_tool.add_separator()
        m_tool.add_command(label="删除索引缓存", command=self._delete_cache)
        menubar.add_cascade(label="工具", menu=m_tool)

        m_help = tk.Menu(menubar, tearoff=0)
        m_help.add_command(label="关于", command=self._about)
        menubar.add_cascade(label="帮助", menu=m_help)

        self.root.config(menu=menubar)

    def _build_widgets(self):
        # 搜索框
        top = ttk.Frame(self.root, padding=(8, 8, 8, 4))
        top.pack(fill="x")
        self.search_var = tk.StringVar()
        self.search_var.trace_add("write", self._on_search_changed)
        self.search_entry = ttk.Entry(top, textvariable=self.search_var, font=("Microsoft YaHei UI", 12))
        self.search_entry.pack(fill="x", ipady=3)
        # 占位提示（首次输入时自动清除，且不吞掉用户敲下的第一个字符）
        self._placeholder_on = False
        self._search_job = None
        self._placeholder_text = "输入文件名或路径关键词，多关键词用空格分隔…"
        self.search_entry.bind("<KeyPress>", self._on_entry_keypress, add="+")
        self.search_entry.bind("<FocusOut>", lambda e: self._show_placeholder(), add="+")
        self._show_placeholder()

        # 结果列表
        columns = ("name", "path", "size", "mtime")
        frame = ttk.Frame(self.root)
        frame.pack(fill="both", expand=True, padx=8, pady=4)

        self.tree = ttk.Treeview(frame, columns=columns, show="headings", selectmode="browse")
        headers = {"name": ("名称", 260), "path": ("路径", 420), "size": ("大小", 90), "mtime": ("修改时间", 130)}
        for col in columns:
            text, width = headers[col]
            anchor = "e" if col == "size" else "w"
            self.tree.heading(col, text=text, command=lambda c=col: self._sort_by(c))
            self.tree.column(col, width=width, anchor=anchor, stretch=(col in ("name", "path")))

        vsb = ttk.Scrollbar(frame, orient="vertical", command=self._on_scroll)
        self.tree.configure(yscrollcommand=vsb.set)
        self.tree.pack(side="left", fill="both", expand=True)
        vsb.pack(side="right", fill="y")

        # 右键菜单
        self.ctx_menu = tk.Menu(self.root, tearoff=0)
        self.ctx_menu.add_command(label="打开", command=self._open_selected)
        self.ctx_menu.add_command(label="打开所在文件夹", command=self._open_containing)
        self.ctx_menu.add_command(label="复制完整路径", command=self._copy_path)

        # 状态栏
        self.status_var = tk.StringVar(value="就绪")
        status = ttk.Label(self.root, textvariable=self.status_var, anchor="w", padding=(8, 2))
        status.pack(fill="x", side="bottom")

    def _bind_events(self):
        self.tree.bind("<Double-1>", lambda e: self._open_selected())
        self.tree.bind("<Return>", lambda e: self._open_selected())
        self.tree.bind("<Button-3>", self._show_ctx_menu)

    # ---------- 占位符 ----------

    def _show_placeholder(self):
        if not self.search_var.get():
            self._placeholder_on = True
            self.search_entry.config(foreground="gray")
            self.search_var.set(self._placeholder_text)
            self.search_entry.icursor(0)

    def _clear_placeholder(self):
        self._placeholder_on = False
        self.search_entry.config(foreground="black")
        self.search_var.set("")

    def _on_entry_keypress(self, event):
        """占位文本存在时，先清空再让按键正常输入（避免吞掉第一个字符）。"""
        navigation = {"Left", "Right", "Home", "End", "Up", "Down", "Shift_L", "Shift_R",
                      "Control_L", "Control_R", "Tab", "Escape"}
        if self._placeholder_on and event.keysym not in navigation:
            self._clear_placeholder()
        return None

    def _clear_search(self):
        self._clear_placeholder()
        self.search_var.set("")
        self._placeholder_on = False
        self._show_placeholder()
        self.search_entry.focus_set()

    # ---------- 搜索 ----------

    def _on_search_changed(self, *_):
        # 占位文本变化不触发搜索
        if self._placeholder_on:
            return
        if self._search_job is not None:
            self.root.after_cancel(self._search_job)
        self._search_job = self.root.after(60, self._do_search)

    def _do_search(self):
        raw = self.search_var.get()
        tokens = [t.lower() for t in raw.split() if t]
        if not tokens:
            self.results = []
            self._reload_tree()
            self.status_var.set(f"索引共 {len(self.index):,} 项，输入关键词开始搜索")
            return
        t0 = time.perf_counter()
        results = [r for r in self.index if match(tokens, r[0], r[2].lower())]
        elapsed = (time.perf_counter() - t0) * 1000
        # 目录排在文件后面不强制，保持磁盘顺序；默认显示前 PAGE_SIZE 条
        self.results = results
        self._reload_tree()
        self.status_var.set(f"{len(results):,} 个对象 （{elapsed:.1f} ms）")

    # ---------- 列表渲染（虚拟滚动）----------

    def _reload_tree(self):
        self.tree.delete(*self.tree.get_children())
        self.shown = 0
        self._append_rows()

    def _append_rows(self):
        batch = self.results[self.shown:self.shown + PAGE_SIZE]
        for r in batch:
            _, name, path, size, mtime, is_dir = r
            tag = "dir" if is_dir else "file"
            self.tree.insert("", "end", values=(name, path, fmt_size(size), fmt_mtime(mtime)), tags=(tag,))
        self.shown += len(batch)
        self.tree.tag_configure("dir", foreground="#1a5fb4")

    def _on_scroll(self, first, _last):
        self.tree.yview_moveto(first)
        # 滚动到接近底部时加载更多
        if float(first) > 0.85 and self.shown < len(self.results):
            self._append_rows()

    # ---------- 排序 ----------

    def _sort_by(self, col):
        reverse = self.sort_state["col"] == col and not self.sort_state["reverse"]
        self.sort_state = {"col": col, "reverse": reverse}
        keymap = {
            "name": lambda r: r[1].lower(),
            "path": lambda r: r[2].lower(),
            "size": lambda r: r[3],
            "mtime": lambda r: r[4],
        }
        self.results.sort(key=keymap[col], reverse=reverse)
        self._reload_tree()
        self.status_var.set(f"已按 {self.tree.heading(col)['text']} "
                            f"{'降序' if reverse else '升序'} 排序，共 {len(self.results):,} 个对象")

    # ---------- 索引 ----------

    def _change_scope(self):
        if self.indexing:
            messagebox.showinfo("提示", "正在索引中，请先取消索引（菜单 搜索-取消索引）")
            return
        if self._choose_scope():
            self.reindex()

    def reindex(self):
        if self.indexing:
            return
        if not self.scanned_roots:
            if not self._choose_scope(first_run=True):
                return
        self.indexing = True
        self.cancel_event.clear()
        roots = list(self.scanned_roots)
        # 注意：不清空 self.index —— 刷新期间旧索引仍可搜索（stale-while-revalidate）
        self.status_var.set("正在建立索引（后台运行，可继续搜索已有索引）…")
        threading.Thread(target=self._index_worker, args=(roots,), daemon=True).start()

    def _index_worker(self, roots):
        """逐盘索引：每完成一个磁盘立即落盘，中途关窗/取消也不丢已有进度。"""
        t0 = time.time()
        accumulated = []
        completed_roots = []
        try:
            for i, root in enumerate(roots, 1):
                if self.cancel_event.is_set():
                    break
                self._set_status(f"正在索引 {root}（{i}/{len(roots)}）… 已累计 {len(accumulated):,} 项")
                part = build_index([root], progress_cb=self._index_progress,
                                   cancel_event=self.cancel_event)
                accumulated.extend(part)
                # 每完成一个磁盘就更新可用索引，便于边索引边搜索
                self.index = accumulated
                if not self.cancel_event.is_set():
                    completed_roots.append(root)
                    save_cache(accumulated, time.time())   # 逐盘落盘
            self.index = accumulated
            if self.cancel_event.is_set():
                # 取消/关窗：保存已完成的部分
                if accumulated:
                    save_cache(accumulated, time.time())
                self._set_status(f"索引已停止：{len(accumulated):,} 项已保存，可用于搜索")
            else:
                self._set_status(f"索引完成：{len(accumulated):,} 项，"
                                 f"共 {len(completed_roots)} 个磁盘，耗时 {time.time() - t0:.0f} 秒（已缓存）")
        finally:
            self.indexing = False

    def _index_progress(self, count):
        self._set_status(f"正在索引… 当前磁盘已 {count:,} 项（后台运行，可继续搜索）")

    def _set_status(self, text):
        self.root.after(0, lambda: self.status_var.set(text))

    def _cancel_index(self):
        self.cancel_event.set()

    def _on_close(self):
        if self.indexing:
            self.cancel_event.set()
            self.status_var.set("正在保存索引进度…")
            # 等待工作线程把已完成部分落盘，最多 3 秒
            deadline = time.time() + 3
            while self.indexing and time.time() < deadline:
                self.root.update()
                time.sleep(0.05)
        self.root.destroy()

    def _delete_cache(self):
        try:
            if os.path.exists(CACHE_FILE):
                os.remove(CACHE_FILE)
            self.status_var.set("索引缓存已删除（下次启动需重新索引）")
        except OSError as e:
            messagebox.showerror("错误", f"删除缓存失败：{e}")

    # ---------- 操作 ----------

    def _selected_record(self):
        sel = self.tree.selection()
        if not sel:
            return None
        values = self.tree.item(sel[0], "values")
        path = values[1]
        for r in self.results:
            if r[2] == path:
                return r
        return None

    def _open_selected(self):
        rec = self._selected_record()
        if rec:
            self._open_path(rec[2], rec[5])

    def _open_containing(self):
        rec = self._selected_record()
        if rec:
            folder = os.path.dirname(rec[2])
            self._open_path(folder, True)

    def _open_path(self, path, is_dir):
        try:
            if is_dir:
                subprocess.Popen(["explorer", os.path.normpath(path)])
            else:
                os.startfile(os.path.normpath(path))
        except OSError as e:
            messagebox.showerror("打开失败", str(e))

    def _copy_path(self):
        rec = self._selected_record()
        if rec:
            self.root.clipboard_clear()
            self.root.clipboard_append(os.path.normpath(rec[2]))
            self.status_var.set("路径已复制到剪贴板")

    def _show_ctx_menu(self, event):
        item = self.tree.identify_row(event.y)
        if item:
            self.tree.selection_set(item)
            self.ctx_menu.tk_popup(event.x_root, event.y_root)

    def _about(self):
        messagebox.showinfo("关于 FastSearch",
                            "FastSearch —— 仿 Everything 的快速文件搜索工具\n\n"
                            "· 后台遍历磁盘建立索引并缓存 24 小时\n"
                            "· 输入即搜，支持多关键词（空格分隔）\n"
                            "· 双击打开，右键更多操作，点击表头排序\n"
                            "· 无需管理员权限")


def main():
    # Windows 下任务栏图标分组
    try:
        ctypes.windll.shell32.SetCurrentProcessExplicitAppUserModelID("fastsearch.app")
    except Exception:
        pass
    root = tk.Tk()
    FastSearchApp(root)
    root.mainloop()


if __name__ == "__main__":
    main()
