#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // When invoked as a privileged helper by sudo, write stdin to a device.
    // This avoids needing a separate signed helper binary.
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("--privileged-flash") if args.len() == 3 => {
            dd_etcher_lib::privileged_flash(&args[2]);
            return;
        }
        Some("--privileged-sha256") if args.len() == 4 => {
            let limit: u64 = args[3].parse().unwrap_or(0);
            dd_etcher_lib::privileged_sha256(&args[2], limit);
            return;
        }
        _ => {}
    }
    dd_etcher_lib::run()
}
