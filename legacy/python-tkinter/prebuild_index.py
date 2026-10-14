# -*- coding: utf-8 -*-
"""预建索引脚本：为指定磁盘建立索引并落盘缓存（供首次启动秒开）。
用法: python prebuild_index.py [盘符...]   默认仅 C:\
"""
import sys, os, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import fast_search as fs

roots = sys.argv[1:] or ["C:\\"]
roots = [r if r.endswith("\\") else r + "\\" for r in roots]

cfg = fs.load_config()
cfg["roots"] = roots
fs.save_config(cfg)
print("索引范围:", roots, flush=True)

t0 = time.time()
index = []
for i, root in enumerate(roots, 1):
    print(f"[{i}/{len(roots)}] 正在索引 {root} ...", flush=True)
    acc = [0]

    def cb(n, acc=acc):
        if n - acc[0] >= 200000:
            acc[0] = n
            print(f"    {root} 已 {n:,} 项 ({time.time()-t0:.0f}s)", flush=True)

    part = fs.build_index([root], progress_cb=cb)
    index.extend(part)
    fs.save_cache(index, time.time())
    print(f"    {root} 完成: {len(part):,} 项 (累计 {len(index):,})", flush=True)

print(f"全部完成: {len(index):,} 项, 耗时 {time.time()-t0:.0f} 秒", flush=True)
print("缓存文件:", fs.CACHE_FILE, os.path.getsize(fs.CACHE_FILE) // 1024 // 1024, "MB", flush=True)
