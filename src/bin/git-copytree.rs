//! `git copytree <command>`: git runs `git-copytree` from PATH.
fn main() {
    let args = std::iter::once(std::ffi::OsString::from("git copytree"))
        .chain(std::env::args_os().skip(1));
    std::process::exit(copytree::cli::main_with(args));
}
