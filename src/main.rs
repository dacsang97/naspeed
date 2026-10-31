use std::ffi::OsString;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    naspeed::init_logging();

    // The zig shims' `ar` / `lib` / `*dlltool` links point at this binary.
    if let Some(zig) = args.first().and_then(|argv0| {
        let rest: Vec<String> = args[1..]
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        naspeed::cross::linker_shim::zig_tool_for_argv0(std::path::Path::new(argv0), &rest)
    }) {
        return match zig.execute() {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("naspeed: {err:#}");
                ExitCode::from(1)
            }
        };
    }

    // Cargo calls `naspeed rustc <args>`: anything that is not one of our own
    // subcommands or flags means wrapper mode.
    // Started as `ns`: always the CLI, never wrapper mode.
    let as_ns = args.first().is_some_and(|a| naspeed::cli::invoked_as_ns(a));
    if !as_ns && args.len() > 1 && !naspeed::cli::is_naspeed_invocation(&args[1]) {
        let code = naspeed::wrapper::run(&args[1..]);
        return ExitCode::from(code.clamp(0, 255) as u8);
    }

    match naspeed::cli::run(args) {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("naspeed: {err:#}");
            ExitCode::from(1)
        }
    }
}
