#!/usr/bin/env python3
"""把应用图标作为 Windows 资源写进已编译好的 mftfind.exe。

为什么需要这一步：资源管理器/开始菜单里看到的 exe 图标来自 PE 的 `.rsrc` 节，
而写这个节需要资源编译器（MSVC 的 rc.exe，或 MinGW 的 windres）。
这台机器两者都没有，本机测过 GNU ld 也不认 `.res` 文件
（`file format not recognized`），所以只能自己往 PE 里加节。

本脚本**只用标准库**——它会在每次构建时跑，不能要求 Pillow。
要写的资源内容由 tools/make_icon.py 预先画好，打包成 assets/icon.rsrc 载荷：
    [magic 'MFRS'][fixup 数 u32][fixup × (u32, u32)][节内容]
节内容里所有 DATA_ENTRY.OffsetToData 都是 0 占位，fixup 表记录它们在载荷里的
位置以及对应数据的节内偏移，等节的 RVA 确定后再回填。

做法：在文件末尾追加一个 `.rsrc` 节，并把节头、SizeOfImage、
DataDirectory[2] 一并改好。

用法：python tools/embed_icon.py [exe路径]（默认 target/release/mftfind.exe）
"""
from __future__ import annotations

import os
import struct
import sys

# 资源类型
RT_ICON = 3
RT_GROUP_ICON = 14
LANG_EN_US = 0x0409
PAYLOAD_MAGIC = b"MFRS"

IMAGE_SCN_CNT_INITIALIZED_DATA = 0x00000040
IMAGE_SCN_MEM_READ = 0x40000000
IMAGE_DIRECTORY_ENTRY_RESOURCE = 2


def align_up(v: int, a: int) -> int:
    return (v + a - 1) // a * a


# ---------------------------------------------------------------- 载荷

def load_payload(path: str) -> tuple[bytes, list[tuple[int, int]]]:
    d = open(path, "rb").read()
    if d[:4] != PAYLOAD_MAGIC:
        raise SystemExit(f"{path}: 载荷格式不对 —— 先跑 tools/make_icon.py 重新生成")
    count = struct.unpack_from("<I", d, 4)[0]
    fixups = [struct.unpack_from("<II", d, 8 + 8 * i) for i in range(count)]
    return d[8 + 8 * count:], fixups


def with_rva(body: bytes, fixups: list[tuple[int, int]], rva: int) -> bytes:
    """按最终的节 RVA 回填所有 DATA_ENTRY.OffsetToData。"""
    buf = bytearray(body)
    for de_off, data_off in fixups:
        struct.pack_into("<I", buf, de_off, rva + data_off)
    return bytes(buf)


# ---------------------------------------------------------------- PE 加节

def embed(exe: str, make_rsrc) -> tuple[int, bytes, bool]:
    """给 exe 追加 .rsrc 节（含图标）。

    make_rsrc 是个回调，接收算好的节 RVA、返回节内容 —— DATA_ENTRY.OffsetToData
    要写 RVA，而 RVA 得等节布局定下来才知道。只在这里算一次 RVA，避免调用方
    再算一遍、两者对不上。

    幂等：文件末尾若已有本脚本加的 .rsrc 节就先摘掉再加。因为
    `cargo build` 在源码没变时不会重新链接，exe 会保留上次嵌好的资源，
    不处理的话重复构建就会撞上「已经有 .rsrc 节」。
    """
    data = bytearray(open(exe, "rb").read())

    pe = struct.unpack_from("<I", data, 0x3C)[0]
    if data[pe:pe + 4] != b"PE\x00\x00":
        raise SystemExit(f"{exe}: 不是 PE 文件")

    coff = pe + 4
    machine, nsec = struct.unpack_from("<HH", data, coff)
    optsz = struct.unpack_from("<H", data, coff + 16)[0]
    if machine != 0x8664:
        raise SystemExit(f"只支持 x64（machine=0x{machine:X}）")
    opt = coff + 20
    if struct.unpack_from("<H", data, opt)[0] != 0x20B:
        raise SystemExit("只支持 PE32+")

    sect_align = struct.unpack_from("<I", data, opt + 32)[0]
    file_align = struct.unpack_from("<I", data, opt + 36)[0]
    dd = opt + 112
    sect_tbl = opt + optsz

    def read_sections() -> list[tuple[str, int, int, int, int, int]]:
        out = []
        for i in range(nsec):
            o = sect_tbl + 40 * i
            name = bytes(data[o:o + 8]).rstrip(b"\x00").decode("ascii", "replace")
            vsize, va, rsize, raw = struct.unpack_from("<IIII", data, o + 8)
            out.append((name, o, vsize, va, rsize, raw))
        return out

    stripped = 0
    for name, _, _, _, rsize, raw in read_sections():
        if name != ".rsrc":
            continue
        if raw + rsize != len(data):
            raise SystemExit("存在 .rsrc 节但它不在文件末尾 —— 不是本脚本加的，放弃")
        del data[raw:]                       # 我们总是追加在末尾，切掉即可
        data[sect_tbl + 40 * (nsec - 1):sect_tbl + 40 * nsec] = b"\x00" * 40
        nsec -= 1
        struct.pack_into("<H", data, coff + 2, nsec)
        stripped = rsize

    sections = read_sections()
    if stripped:
        # 节被切掉了，头部里指向它的两处（资源目录项、SizeOfImage）也要复位，
        # 否则下面的检查会看到「DataDirectory[2] 非空但没有 .rsrc 节」
        struct.pack_into("<II", data, dd + 8 * IMAGE_DIRECTORY_ENTRY_RESOURCE, 0, 0)
        struct.pack_into("<I", data, opt + 56,
                         align_up(max(s[3] + max(s[2], s[4]) for s in sections), sect_align))

    if any(s[0] == ".rsrc" for s in sections):
        raise SystemExit("还有别的 .rsrc 节，结构不符预期，放弃")

    cur_rva, cur_size = struct.unpack_from("<II", data, dd + 8 * IMAGE_DIRECTORY_ENTRY_RESOURCE)
    if cur_rva or cur_size:
        raise SystemExit("DataDirectory[2] 非空但没有 .rsrc 节，结构不符预期，放弃")

    # 新节头要写进节表末尾，确认那里到第一个节的数据之间有空位
    tbl_end = sect_tbl + 40 * (nsec + 1)
    first_raw = min(s[5] for s in sections if s[5])
    if tbl_end > first_raw:
        raise SystemExit(f"节表没有空间（需要到 0x{tbl_end:X}，首个节数据在 0x{first_raw:X}）")

    # 新节的原始数据接在文件末尾，而不是「最后一个节数据结束处」。
    # gnu 工具链会在 PE 里留一张 COFF 符号表（本项目 release 约 24750 条、
    # 连字符串表近 3MB），它排在所有节之后；按节尾定位会和实际写入位置错开，
    # 结果就是节头指向符号表、资源解析不出来。接在文件末尾则任何情况都对。
    # PointerToSymbolTable 是绝对文件偏移，追加不影响它。
    new_raw = align_up(len(data), file_align)
    new_rva = align_up(max(s[3] + max(s[2], s[4]) for s in sections), sect_align)

    if struct.unpack_from("<I", data, opt + 108)[0] <= IMAGE_DIRECTORY_ENTRY_RESOURCE:
        raise SystemExit("NumberOfRvaAndSizes 太小，放不下资源目录项")

    rsrc = make_rsrc(new_rva)
    raw_size = align_up(len(rsrc), file_align)

    hdr = struct.pack(
        "<8sIIIIIIHHI",
        b".rsrc", len(rsrc), new_rva, raw_size, new_raw,
        0, 0, 0, 0,
        IMAGE_SCN_CNT_INITIALIZED_DATA | IMAGE_SCN_MEM_READ,
    )
    if len(hdr) != 40:
        raise SystemExit("节头长度不对")
    data[tbl_end - 40:tbl_end] = hdr

    struct.pack_into("<H", data, coff + 2, nsec + 1)
    struct.pack_into("<I", data, opt + 56, new_rva + align_up(len(rsrc), sect_align))
    struct.pack_into("<II", data, dd + 8 * IMAGE_DIRECTORY_ENTRY_RESOURCE, new_rva, len(rsrc))
    struct.pack_into("<I", data, opt + 64, 0)         # CheckSum：用户态 exe 不校验，清零

    if len(data) < new_raw:
        data.extend(b"\x00" * (new_raw - len(data)))
    data.extend(rsrc)
    data.extend(b"\x00" * (raw_size - len(rsrc)))

    with open(exe, "wb") as f:
        f.write(data)

    return new_rva, rsrc, bool(stripped)


# ---------------------------------------------------------------- 复核

def verify(exe: str) -> None:
    """独立复核：从 PE 结构出发把资源目录重新走一遍。

    刻意不复用写入时的任何中间变量 —— 只有按规范重新读回来，才能证明写到
    磁盘上的字节真的能被 Windows 解析。手写 PE 最怕的就是「写的时候自认为对」。
    """
    data = open(exe, "rb").read()
    pe = struct.unpack_from("<I", data, 0x3C)[0]
    coff = pe + 4
    nsec = struct.unpack_from("<H", data, coff + 2)[0]
    optsz = struct.unpack_from("<H", data, coff + 16)[0]
    opt = coff + 20
    rva, size = struct.unpack_from("<II", data, opt + 112 + 8 * IMAGE_DIRECTORY_ENTRY_RESOURCE)
    if not rva or not size:
        raise SystemExit("复核失败：DataDirectory[2] 是空的")

    sect_tbl = opt + optsz
    sec = va = None
    for i in range(nsec):
        o = sect_tbl + 40 * i
        if bytes(data[o:o + 8]).rstrip(b"\x00") != b".rsrc":
            continue
        vsize, va, rsize, raw = struct.unpack_from("<IIII", data, o + 8)
        if not va <= rva < va + vsize:
            raise SystemExit(f"复核失败：资源目录 RVA 0x{rva:X} 不在 .rsrc 节范围里")
        if raw + rsize > len(data):
            raise SystemExit("复核失败：.rsrc 节数据超出文件")
        sec = raw
    if sec is None:
        raise SystemExit("复核失败：文件里没有 .rsrc 节")

    # 资源里有两套偏移，别混：目录项的是「相对资源节起始」的节内偏移，
    # 只有 DATA_ENTRY.OffsetToData 才是相对 image base 的 RVA。
    def dir_at(off: int) -> int:
        return sec + off

    def rva_at(x: int) -> int:
        return sec + (x - va)

    found: dict[int, list[int]] = {}
    data_rva: dict[int, int] = {}
    named, ids = struct.unpack_from("<HH", data, dir_at(12))
    for k in range(named + ids):
        name, off = struct.unpack_from("<II", data, dir_at(16 + 8 * k))
        if not off & 0x80000000:
            continue
        sub = off & 0x7FFFFFFF                        # 类型层
        _, n2 = struct.unpack_from("<HH", data, dir_at(sub + 12))
        found.setdefault(name, [])
        for j in range(n2):
            ename, eoff = struct.unpack_from("<II", data, dir_at(sub + 16 + 8 * j))
            if not eoff & 0x80000000:
                continue
            lang = eoff & 0x7FFFFFFF                  # 语言层
            # 目录项是 (Name, OffsetToData) 两个 DWORD，别把 Name 当成偏移用了
            _, de = struct.unpack_from("<II", data, dir_at(lang + 16))
            drva, dsize = struct.unpack_from("<II", data, dir_at(de))
            if not rva <= drva < rva + size:
                raise SystemExit(f"复核失败：类型 {name} 的数据 RVA 0x{drva:X} 越界")
            if dsize == 0 or rva_at(drva) + dsize > len(data):
                raise SystemExit(f"复核失败：类型 {name} 的数据长度 {dsize} 不合法")
            found[name].append(ename)
            data_rva.setdefault(name, drva)

    icons = found.get(RT_ICON, [])
    groups = found.get(RT_GROUP_ICON, [])
    if not icons or not groups:
        raise SystemExit(f"复核失败：资源表里没有图标，实际找到 {found}")

    # GRPICONDIR 的 count 字段（第 3 个 WORD）应当等于 RT_ICON 的条目数，
    # 这是「图标组和图标数据是配套的」最直接的一条证据
    grp_count = struct.unpack_from("<H", data, rva_at(data_rva[RT_GROUP_ICON]) + 4)[0]
    if grp_count != len(icons):
        raise SystemExit(f"复核失败：图标组声明 {grp_count} 个，实际 {len(icons)} 个")

    print(f"  复核通过：RT_ICON {len(icons)} 个、RT_GROUP_ICON {len(groups)} 个，"
          f"语言层 0x{LANG_EN_US:04X}，DataDirectory[2] RVA=0x{rva:X} size={size}")


def main() -> None:
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    exe = sys.argv[1] if len(sys.argv) > 1 else os.path.join(root, "target/release/mftfind.exe")
    if not os.path.exists(exe):
        raise SystemExit(f"找不到 {exe}，先跑 ./build.sh")

    body, fixups = load_payload(os.path.join(root, "assets/icon.rsrc"))

    rva, rsrc, replaced = embed(exe, lambda r: with_rva(body, fixups, r))

    print(f"{os.path.relpath(exe, root)}")
    print(f"  资源载荷 {len(body)} 字节、{len(fixups)} 个数据条目"
          + ("（替换掉上一次的）" if replaced else ""))
    print(f"  .rsrc RVA=0x{rva:X}  节内容 {len(rsrc)} 字节  "
          f"文件 {os.path.getsize(exe)} 字节")
    verify(exe)


if __name__ == "__main__":
    main()
