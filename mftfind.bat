@echo off
rem mftfind 启动器
rem
rem 普通启动：MFT 被拒绝，自动回退并行遍历（约 4~5 秒/百万文件）
rem 想要 MFT 快路径（约 1~3 秒/百万文件）：启动后在界面里点「以管理员身份重启」，会弹 UAC
rem
rem release 版是 windows 子系统程序，本身不会再弹出控制台黑窗。
rem 但双击 .bat 仍会先闪一下 cmd.exe 自己的窗口——想彻底没有闪烁，
rem 直接给 target\release\mftfind.exe 建个快捷方式，绕开 .bat。
start "" "%~dp0target\release\mftfind.exe"
