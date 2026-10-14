@echo off
rem mftfind 启动器
rem 普通启动：MFT 被拒绝，自动回退并行遍历（约 4~5 秒/百万文件）
rem 想要 MFT 快路径（约 1~3 秒/百万文件）：启动后在界面里点「以管理员身份重启」，会弹 UAC
start "" "%~dp0target\release\mftfind.exe"
