#!/usr/bin/env python3
"""生成 mftfind 的应用图标。

产物：
  assets/icon.ico     多尺寸 Windows 图标（16/24/32/48/64/128/256）
  assets/icon.rgba    128x128 裸 RGBA，Rust 侧 include_bytes! 后直接喂 egui::IconData
  assets/icon.rsrc    资源载荷，供 tools/embed_icon.py 写进 exe 的 .rsrc 节
  assets/preview.png  256px 预览，只为人眼看，不参与构建

icon.rsrc 的格式（embed_icon.py 只用标准库读，所以 Pillow 相关的活全在这做）：
    [magic 'MFRS'][fixup 数 u32][fixup × (u32, u32)][节内容]
节内容是完整的 .rsrc 节布局，其中所有 DATA_ENTRY.OffsetToData 都是 0 占位；
fixup 表记录每个 DATA_ENTRY 的节内偏移和对应数据的节内偏移，等节的 RVA
在嵌入时确定后再回填。

造型：深青圆角方块 + 白色放大镜，镜片内三条左对齐、长度递减的横线（搜索结果列表）。

为什么小尺寸要单独一套参数：16px 下环只有约 1.3px 宽，三条横线会糊成一个灰疙瘩，
放大镜本身也缩到 8px 失去辨识度。所以 <=32px 直接丢掉横线、加粗笔画、减小边距，
让放大镜占更满。这是图标设计的常规做法，不是为了省事。

改配色或造型只需要动下面的常量。
"""
from __future__ import annotations

import io
import io
import os
import struct
from PIL import Image, ImageDraw

BG = (15, 110, 86, 255)      # #0F6E56 深青，和界面灰蓝调拉开
FG = (255, 255, 255, 255)

SS = 4                        # 超采样倍数，缩回来得到干净的抗锯齿边缘
SIZES = (16, 24, 32, 48, 64, 128, 256)
RGBA_SIZE = 128

# 放大镜环内那三条横线：(相对环心的 y 偏移, 起点 x, 终点 x)，基准 256
LINES = ((-24, 80, 126), (0, 80, 130), (24, 80, 120))

# Windows 资源里的固定常量
RT_ICON = 3
RT_GROUP_ICON = 14
LANG_EN_US = 0x0409
PAYLOAD_MAGIC = b"MFRS"


def draw(size: int) -> Image.Image:
    """在 size*SS 的画布上绘制，调用方负责缩回 size。"""
    n = size * SS
    img = Image.new("RGBA", (n, n), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    compact = size <= 32

    if compact:
        pad = n * 4 / 256
        cx, cy = n * 0.400, n * 0.395
        r_out, ring_w = n * 0.255, n * 0.115
        hx, hy, handle_w = n * 0.815, n * 0.810, n * 0.120
    else:
        pad = n * 6 / 256
        cx, cy = n * 100 / 256, n * 98 / 256
        r_out, ring_w = n * 62 / 256, n * 18 / 256
        hx, hy, handle_w = n * 204 / 256, n * 202 / 256, n * 22 / 256

    d.rounded_rectangle([pad, pad, n - pad, n - pad], radius=n * 56 / 256, fill=BG)

    # 手柄先画、环后画：环的不透明描边会盖住接缝。
    # 手柄圆头半径 = handle_w/2 = 11（基准 256），起点在 r_out=62 的圆上，
    # 所以圆头最多伸到离环心 51 处，而环内半径是 62-18=44 —— 不会捅进镜片里。
    start = (cx + r_out * 0.70711, cy + r_out * 0.70711)
    d.line([start, (hx, hy)], fill=FG, width=round(handle_w))
    r = handle_w / 2
    d.ellipse([hx - r, hy - r, hx + r, hy + r], fill=FG)

    d.ellipse([cx - r_out, cy - r_out, cx + r_out, cy + r_out],
              outline=FG, width=round(ring_w))

    if not compact:
        lw = round(n * 11 / 256)
        for dy, x0, x1 in LINES:
            y = cy + n * dy / 256
            d.line([(n * x0 / 256, y), (n * x1 / 256, y)], fill=FG, width=lw)

    return img.resize((size, size), Image.LANCZOS)


def write_ico(path: str, images: dict[int, Image.Image]) -> int:
    """手写 ICO 容器。

    不用 Pillow 的 ICO 保存是因为它只接受一张源图、自己缩放出其余尺寸，
    缩放算法不可控；这里各尺寸都是单独绘制再 LANCZOS 缩小的，质量更好。
    每个条目按 PNG 存（Vista 起支持），256px 也不会撑爆文件。
    """
    entries, blobs, offset = [], [], 6 + 16 * len(images)
    for size, im in sorted(images.items()):
        buf = io.BytesIO()
        im.save(buf, format="PNG")
        blob = buf.getvalue()
        b = 0 if size >= 256 else size        # 0 表示 256
        entries.append(struct.pack("<BBBBHHII", b, b, 0, 0, 1, 32, len(blob), offset))
        blobs.append(blob)
        offset += len(blob)

    with open(path, "wb") as f:
        f.write(struct.pack("<HHH", 0, 1, len(images)))
        for e in entries:
            f.write(e)
        for blob in blobs:
            f.write(blob)
    return offset


# ------------------------------------------------------------- 图标 -> 资源

def icon_bitmap(png: bytes) -> tuple[bytes, int, int]:
    """把一张 PNG 转成 RT_ICON 要求的格式。

    格式是 BITMAPINFOHEADER + XOR 位图 + AND 掩码，且两个位图都是自底向上。
    32bpp 下 AND 掩码实际不起作用（透明由 alpha 决定），但结构上必须存在，
    否则资源管理器读图标时会错位。
    """
    im = Image.open(io.BytesIO(png)).convert("RGBA")
    w, h = im.size
    px = im.load()

    xor = bytearray()
    for y in range(h - 1, -1, -1):          # 自底向上
        for x in range(w):
            r, g, b, a = px[x, y]
            xor += bytes((b, g, r, a))      # BGRA

    and_row = ((w + 31) // 32) * 4
    and_mask = bytes(and_row * h)           # 全 0：透明度交给 alpha 通道

    header = struct.pack("<IiiHHIIiiII", 40, w, h * 2, 1, 32, 0, 0, 0, 0, 0, 0)
    return header + bytes(xor) + and_mask, w, h


def group_icon(entries: list[tuple[int, int, int, int]]) -> bytes:
    """GRPICONDIR：把一组 RT_ICON 串成「一个图标」。"""
    out = struct.pack("<HHH", 0, 1, len(entries))
    for icon_id, w, h, size in entries:
        out += struct.pack(
            "<BBBBHHIH",
            0 if w >= 256 else w,           # 256 在 ICO/GRP 格式里用 0 表示
            0 if h >= 256 else h,
            0, 0, 1, 32, size, icon_id,
        )
    return out


def build_rsrc_payload(icon_blobs: list[bytes], group_blob: bytes) -> bytes:
    """构造 .rsrc 节内容，DATA_ENTRY.OffsetToData 先留 0 占位。

    三层目录：根 → 类型(RT_ICON / RT_GROUP_ICON) → ID → 语言 → DATA_ENTRY。
    目录项的偏移是「相对本资源节起始」的节内偏移，这部分现在就能算准；
    只有 DATA_ENTRY.OffsetToData 要的是相对 image base 的 RVA，得等节的
    RVA 在嵌入时确定，所以先记进 fixup 表，交给 embed_icon.py 回填。
    """
    buf = bytearray()
    fixups: list[tuple[int, int]] = []
    n = len(icon_blobs)

    def alloc(size: int) -> int:
        while len(buf) % 4:
            buf.append(0)
        off = len(buf)
        buf.extend(b"\x00" * size)
        return off

    root = alloc(16 + 8 * 2)
    icon_dir = alloc(16 + 8 * n)
    icon_lang = [alloc(16 + 8) for _ in range(n)]
    grp_dir = alloc(16 + 8)
    grp_lang = alloc(16 + 8)
    ents = [alloc(16) for _ in range(n + 1)]

    icon_data = [alloc(len(b)) for b in icon_blobs]
    grp_data = alloc(len(group_blob))

    def header(off: int, count: int) -> None:
        struct.pack_into("<IIHHHH", buf, off, 0, 0, 0, 0, 0, count)

    def entry(off: int, name: int, target: int, is_dir: bool) -> None:
        struct.pack_into("<II", buf, off, name,
                         target | 0x80000000 if is_dir else target)

    header(root, 2)
    entry(root + 16, RT_ICON, icon_dir, True)
    entry(root + 24, RT_GROUP_ICON, grp_dir, True)

    header(icon_dir, n)
    for i in range(n):
        entry(icon_dir + 16 + 8 * i, i + 1, icon_lang[i], True)
    for i in range(n):
        header(icon_lang[i], 1)
        entry(icon_lang[i] + 16, LANG_EN_US, ents[i], False)

    header(grp_dir, 1)
    entry(grp_dir + 16, 1, grp_lang, True)
    header(grp_lang, 1)
    entry(grp_lang + 16, LANG_EN_US, ents[n], False)

    for i in range(n):
        fixups.append((ents[i], icon_data[i]))
        struct.pack_into("<IIII", buf, ents[i], 0, len(icon_blobs[i]), 0, 0)
    fixups.append((ents[n], grp_data))
    struct.pack_into("<IIII", buf, ents[n], 0, len(group_blob), 0, 0)

    for i, blob in enumerate(icon_blobs):
        buf[icon_data[i]:icon_data[i] + len(blob)] = blob
    buf[grp_data:grp_data + len(group_blob)] = group_blob

    return (PAYLOAD_MAGIC + struct.pack("<I", len(fixups))
            + b"".join(struct.pack("<II", a, b) for a, b in fixups)
            + bytes(buf))


def main() -> None:
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    out = os.path.join(root, "assets")
    os.makedirs(out, exist_ok=True)

    imgs = {s: draw(s) for s in SIZES}

    ico = os.path.join(out, "icon.ico")
    size = write_ico(ico, imgs)
    print(f"{os.path.relpath(ico, root)}  {size} bytes")

    rgba = os.path.join(out, "icon.rgba")
    with open(rgba, "wb") as f:
        f.write(imgs[RGBA_SIZE].tobytes())
    print(f"{os.path.relpath(rgba, root)}  {os.path.getsize(rgba)} bytes "
          f"({RGBA_SIZE}x{RGBA_SIZE} RGBA)")

    # 资源载荷：把 ICO 里的每张 PNG 解成 RT_ICON 位图，串成目录树。
    # embed_icon.py 只用标准库，碰不了 PNG，所以这步必须在有 Pillow 的这里做。
    blobs, entries = [], []
    for i, (sz, im) in enumerate(sorted(imgs.items())):
        buf = io.BytesIO()
        im.save(buf, format="PNG")
        blob, w, h = icon_bitmap(buf.getvalue())
        blobs.append(blob)
        entries.append((i + 1, w, h, len(blob)))
    payload = build_rsrc_payload(blobs, group_icon(entries))

    rsrc = os.path.join(out, "icon.rsrc")
    with open(rsrc, "wb") as f:
        f.write(payload)
    print(f"{os.path.relpath(rsrc, root)}  {len(payload)} bytes "
          f"({len(entries)} 个尺寸, {sum(len(b) for b in blobs)} 字节位图)")

    png = os.path.join(out, "preview.png")
    imgs[256].save(png)
    print(f"{os.path.relpath(png, root)}  preview")


if __name__ == "__main__":
    main()
