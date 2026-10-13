#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // On Linux, pkexec re-runs this binary as root only to open the device and
    // pass the descriptor back. macOS needs no helper: authopen(1) does that.
    #[cfg(target_os = "linux")]
    {
        let args: Vec<String> = std::env::args().collect();
        if let [_, flag, device] = args.as_slice() {
            if flag == "--privileged-open" {
                dd_etcher_lib::privileged_open(device);
                return;
            }
        }
    }
    dd_etcher_lib::run()
}
