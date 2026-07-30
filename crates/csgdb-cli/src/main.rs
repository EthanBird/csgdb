use csgdb_core::{OpenOptions, ABI_VERSION, LIB_VERSION};
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut arguments = std::env::args();
    let _program = arguments.next();

    match arguments.next().as_deref() {
        Some("--version" | "version") => {
            println!("csg {LIB_VERSION} (ABI {ABI_VERSION})");
            ExitCode::SUCCESS
        }
        Some("defaults") => {
            let defaults = OpenOptions::default();
            println!("database extension: .db");
            println!("encryption: enabled");
            println!("open flags: 0x{:04x}", defaults.flags.bits());
            println!("key source: {}", defaults.key.kind_name());
            println!("cache bytes: {}", defaults.cache_size_bytes);
            println!("memory budget bytes: {}", defaults.memory_budget_bytes);
            ExitCode::SUCCESS
        }
        Some("-h" | "--help" | "help") | None => {
            print_help();
            ExitCode::SUCCESS
        }
        Some(command) => {
            eprintln!("unknown command: {command}");
            print_help();
            ExitCode::from(2)
        }
    }
}

fn print_help() {
    println!(
        "CSGDB command-line tools\n\n\
         Usage:\n  \
           csg --version\n  \
           csg defaults\n  \
           csg help\n\n\
         Database commands are introduced with the M1 storage backend."
    );
}
