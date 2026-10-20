# mftfind

Windows 上近乎即时的文件名搜索工具，思路源自 [Everything](https://www.voidtools.com/)，
用 Rust 从零实现，小到可以通读一遍。

**这不是 Everything 的替代品，也不比它小。**
Everything 的 64 位便携版压缩包 1.8 MB；本项目的发布包 3.0 MB，解开后的二进制还要更大。
Everything 的索引文件同样是二进制的，所以索引格式也不是差异点。
mftfind 提供的是「更少」的反面：约 3,300 行 Rust 覆盖整条链路——
MFT 枚举、索引布局、查询路径——一个下午能读完，改动起来不用怕。

`mftfind` 通过 `FSCTL_ENUM_USN_DATA` 直接枚举 NTFS 主文件表（MFT），写入自有的二进制索引，
查询时用 mmap 把索引映射进内存，因此搜索的**反序列化开销为零**。

在开发机上，索引覆盖 **7 个卷、9,210,000+ 条记录（672 MB）**，
页缓存命中时一次子串查询约 **150 ms**。

[English](README.md)

---

## 工作原理

两条互相独立的索引路径，自动选择：

| 路径 | 前提 | 吞吐 | 说明 |
| --- | --- | --- | --- |
| **MFT 枚举** | 管理员权限 | 约 1–3 秒 / 百万文件 | 首选；直接读 MFT，不做目录遍历 |
| **并行遍历** | 无 | 约 4–5 秒 / 百万文件 | MFT 句柄被拒或卷类型不支持时的回退 |

未提权时，内置的 `FindFirstFileExW` 遍历器（用 `rayon` 并行）会接管，保证工具始终可用。
界面上有一个 **「以管理员身份重启」** 按钮，点击后弹 UAC 并把进程重新拉回快路径。

有一个取舍值得先知道：**`FSCTL_ENUM_USN_DATA` 既不返回文件大小，也不返回时间戳。**
它的记录描述的是「变更」而非「文件」，微软文档也把 `TimeStamp`、`Reason`、`SourceInfo`
标注为在兄弟接口 `FSCTL_READ_FILE_USN_DATA` 上无效；实测枚举接口行为相同——
对本机 5 个由 MFT 建立的索引采样，**100% 的记录时间戳为 0**。
Everything 绕开这个限制的办法是直接读 `$MFT` 字节流，而不用日志接口。

所以 MFT 路径把这些字段存为**未知**（`size = u64::MAX`、`mtime = 0`），
界面则在滚动时对可见行各做一次 `fs::metadata` 补齐并缓存。
遍历路径的这两个值直接来自 `FindFirstFileExW`，之后不再碰磁盘。
因此**按大小或修改时间排序只在遍历建立的卷上有意义**。

### 索引格式

每个卷一个 `.fst` 文件，设计目标是「映射」而非「解析」：

```
[0..16)   magic  "FSTSEARCH-INDEX1"
[16..20)  version      u32
[20..24)  record count u32
[24..32)  build time   i64（unix 秒）
[32..36)  搜索用名字块长度（ASCII 小写化后的名字）
[36..40)  显示用名字块长度（保留原大小写的名字）
[40..44)  记录区长度（= count * 28）
[44..64)  保留
[64..)    records[count]，每条 28 字节
            0..4   name_off  u32   在「两个」名字块中的偏移
            4..6   name_len  u16
            6..8   flags     u16   bit0=目录 bit1=重解析点 bit2=大小已知
            8..12  parent    u32   父记录下标，u32::MAX 表示根
           12..20  size      u64   未知时为 u64::MAX
           20..28  mtime     i64   unix 秒
          其后依次是小写名字块和原大小写名字块
```

小写化只影响 ASCII（在 UTF-8 下字节长度不变），因此两个名字块始终严格对齐——
每条记录只存一个偏移就够了。父项存的是记录下标，
所以完整路径是查询时沿树向上回溯出来的，不必在磁盘上实体化。

## 用法

```
mftfind                              启动图形界面
mftfind gui                          启动图形界面
mftfind index [C D ...] [--walk]     建立 / 刷新索引（默认：所有固定磁盘）
mftfind search <查询词> [--limit N]   命令行搜索
mftfind bench [C ...]                索引 + 搜索基准测试
mftfind drives                       列出磁盘、权限与索引状态
mftfind mft [C ...]                  直接探测 MFT 枚举（不走遍历回退）
```

`mft` 子命令是诊断入口：它会打印原始的 Win32 错误码，
因此一次失败的索引可以一次性判定到底是权限、卷特性还是枚举逻辑的问题。

### 控制台行为

Release 版按 `IMAGE_SUBSYSTEM_WINDOWS` 链接
（`src/main.rs` 中的 `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`），
所以双击 exe **不会**弹出控制台窗口。
没有这个属性时，二进制默认是控制台子系统，Windows 会在每次启动（包括从资源管理器启动）时分配一个控制台。

这个子系统带来一个副作用：进程创建时**没有标准句柄**，`println!` 会失败并 panic。
`win::init_console` 会在第一次输出前把三个流槽位修好：

- 已经有效的句柄原样保留，所以 `mftfind search x > out.txt` 和管道照常工作；
- CLI 子命令若 stdout 无效，会附加到父控制台并重新打开 `CONOUT$`，
  输出仍能回到启动它的那个终端；
- 图形界面，或完全不存在控制台时，把流指向 `NUL`——输出被丢弃，而不是让进程崩溃。

两个细节值得知道：从命令行启动的 GUI 子系统程序，它的输出会出现在**下一个提示符之后**，
因为 shell 并不会等它；debug 版保留控制台子系统，因此 `cargo run -- bench` 行为正常。

### 界面

- 增量搜索——边输入边收窄结果
- 可按名称 / 修改时间 / 大小 / 类型排序
- 有大小和修改时间两列；在 MFT 建立的卷上，随滚动对可见行从文件系统补齐（见上文）
- 打开结果、在资源管理器中定位、复制完整路径——四个入口任选：工具栏按钮、
  右键菜单、`Enter` / `Ctrl+Enter` / `Ctrl+C`、或双击「路径」列
- `↑` / `↓` 移动选中行，焦点不用离开搜索框
- 分磁盘的索引管理，可选「强制遍历」模式
- 中英文界面，从「语言」菜单切换
- 系统字体回退，中文文件名开箱即可正常显示

### 语言

界面提供中文和英文。启动时的语言取自 `%APPDATA%\mftfind\ui.ini`，
没有则回退到 `GetUserDefaultUILanguage` 查询到的系统界面语言——中文系统得到中文，其余得到英文。
菜单栏里的**「语言」菜单**可即时切换并记住选择；
菜单项用各自的语言书写（中文 / English），即便当前界面语言你看不懂也能找到。

所有文案都在 `src/i18n.rs` 里以两张 `Strings` 表存在，界面代码中不含任何字面标签。
增加一种语言 = 加一个 `Lang` 变体、一张表，以及 `strings` / `endonym` / `code` / `from_code`
各补一个分支——其余代码不动。带参数的消息用 `{}` 模板，让语序差异
（「命中 3 条」vs "3 hits"）留在表内解决；
`cargo test` 会断言两张表占位符个数一致——少一个占位符只会静默丢字段，不会响警报。

两款系统字体是**追加**（而非前插）到 egui 内置字体栈之后，因此拉丁文仍用 egui 自带字体渲染，
回退字体只补它缺的部分：先是 **Segoe UI Symbol** 负责符号，再是 **中文字体**（SimHei 等）。
符号字体不是可选项——egui 0.36 内置字体只覆盖 `☐ ● • · √ ★ ☆ → ⛃ ☜`，
没有它菜单里的勾会渲染成一个空方框。
有一个测试会用真实的字体栈渲染一帧，断言这些字形确实存在。

CLI 子命令目前仍是纯中文——只有界面做了本地化。

## 构建

```sh
./build.sh              # release 构建
./build.sh --debug      # debug 构建
```

`build.sh` 面向 `x86_64-pc-windows-gnu` 工具链，并绕过两个在开发机上实测到的环境坑：

1. `~/.cargo/bin` 下的 `rustc`/`cargo` 代理外壳在本沙箱中会静默失效，
   所以脚本用绝对路径调用工具链本体。
2. `windows-targets` 之类的 crate 在 GNU 目标下使用 `raw-dylib`，编译期需要 `dlltool.exe`。
   它随工具链提供在 `lib/rustlib/x86_64-pc-windows-gnu/bin/self-contained/`，
   加进 `PATH` 时必须用 POSIX 写法，因为 MSYS bash 会按 `C:/...` 里的冒号切碎。

标准 MSVC 工具链同样可用——不借助脚本、直接 `cargo build --release` 也能得到 release 二进制。

### 依赖说明

`eframe` 以 `default-features = false` 引入，只开启 `glow` + `default_fonts`。
默认特性集会把 wayland、X11、accesskit 全都拖进来，而这些在 Windows 上一个都用不到，
裁掉后构建图减少约 150 个 crate。
release profile 里**刻意不设** `codegen-units = 1`：
它对该 profile 下的全部依赖生效，是首次编译耗时的主因，收益只体现在最终二进制上。

### 图标

应用图标由 `tools/make_icon.py`（依赖 Pillow）生成到 `assets/`：

- `icon.rgba` —— 128×128 裸像素，通过 `include_bytes!` 编进二进制，
  再交给 egui 的 `ViewportBuilder::with_icon`。这就是窗口标题栏和任务栏里的图标，
  不需要任何额外依赖。
- `icon.ico` —— 多尺寸容器（16→256），供资源管理器和开始菜单显示的**文件**图标使用。

第二个是麻烦的那半边：把图标写进 PE 的 `.rsrc` 节通常需要资源编译器
（MSVC 是 `rc.exe`，MinGW 是 `windres`），而 GNU `ld` 直接拒绝普通的 `.res` 文件。
因此 `tools/embed_icon.py` 手工把该节追加到已编译的可执行文件上——
节头、`SizeOfImage`、`DataDirectory[2]` 以及三层资源目录全部从零写出——
然后**从磁盘重新解析**结果以确认资源树能够往返。脚本是幂等的，
由 `build.sh` 和 release 工作流自动调用。

小尺寸（≤32 px）会丢掉镜片内的横线并加粗笔画；
16 px 下三条细线会糊成一团灰。

## 目录结构

```
src/
  main.rs      CLI 入口与参数解析
  gui.rs       egui / eframe 界面
  i18n.rs      界面文案与语言选择（中文 / 英文）
  build.rs     索引构建、路径解析、遍历回退
  mft.rs       FSCTL_ENUM_USN_DATA 枚举
  walk.rs      并行 FindFirstFileExW 遍历
  format.rs    二进制索引格式（mmap 读 + 写）
  search.rs    分词、匹配、排序
  win.rs       轻量 Win32 封装（卷句柄、控制台初始化、错误）
assets/        生成的图标（见上文「图标」）
tools/
  make_icon.py     绘制图标并写入 assets/
  embed_icon.py    给编译好的 exe 追加 .rsrc 节
legacy/
  python-tkinter/  最初的 Python 原型，留作参考
```

## 环境要求

- Windows 10 / 11，NTFS 卷
- Rust（stable）——GNU 或 MSVC 工具链均可
- 走 MFT 快路径需要管理员权限（可选）

## 当前状态

早期但可用。已知待办：

- 增量搜索除了防抖重查之外，还没有接进界面的主事件循环；
  后台索引预热仍待实现。
- 页缓存冷启动的查询受 I/O 限制（首次扫描 920 万条记录约 1.4 秒）；
  之后的查询约 150 ms。

## 发布

版本遵循 [SemVer 2.0.0](https://semver.org/)，通过 `vMAJOR.MINOR.PATCH[-CHANNEL.N]`
形式的注解 tag 发布：

```
v0.1.0               正式版
v0.2.0-alpha.1       预发布，功能仍在变动
v0.2.0-beta.1        预发布，功能已冻结
v0.2.0-rc.1          预发布，只修阻塞问题
```

推送这样的 tag 会触发 `.github/workflows/release.yml`：
它会校验 tag 与 `Cargo.toml` 中的 `package.version` 是否一致，
在 Windows runner 上构建，并发布一个附带打包 zip 与校验和的 GitHub Release。

发一个版本：

```sh
./scripts/release.sh 0.2.0-alpha.1 --push
```

完整规则——该升哪一位、什么算破坏性变更（尤其是索引格式变更）、以及 CI 的行为——
见 [RELEASING.md](RELEASING.md)。

## 许可证

尚未选定。
