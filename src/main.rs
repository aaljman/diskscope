#![windows_subsystem = "windows"]

mod app;
mod cli;
mod drives;
mod hotspots;
mod model;
mod scan;
mod theme;
mod treemap;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--cli") {
        cli::run(&args[1..]);
        return;
    }
    app::run(args);
}
