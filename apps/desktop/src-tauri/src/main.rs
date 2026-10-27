// 防止 Windows 发布包上出现额外的控制台窗口。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    wb_desktop::run()
}
