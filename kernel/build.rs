//! Turns `GREY_OS_PANIC_TEST` into `cfg(grey_os_panic_test)`, and `GREY_OS_EXCEPTION_TEST`
//! into one of five fault cfgs.
//!
//! The panic path has to be exercised without a human watching, and the kernel needs
//! to be told to fail for that: `main.rs` gates a single injected `panic!` on this
//! cfg. A cargo feature would remove the need for a build script, but it cannot be
//! reached from the artifact dependency in the root `build.rs` without threading a
//! feature through that manifest, and an environment variable is also what a human
//! types at the shell to see the red panel on purpose (ADR-023):
//!
//! ```text
//! GREY_OS_PANIC_TEST=1 cargo xtask run
//! GREY_OS_EXCEPTION_TEST=gp cargo xtask run
//! ```
//!
//! The second variable exists for the same reason (ADR-028): each of its values compiles in
//! a different instruction the CPU refuses, so the exception path is reached the way it will be
//! in the wild - or, for `guard`, with a write past the heap's guard page, which faults for the
//! same reason a real overrun will. One cfg per fault rather than one cfg carrying a string
//! keeps the test blocks in `main.rs` mutually exclusive at compile time, which is what makes
//! "unreachable after a diverging block" impossible to write by accident.
//!
//! The values are read at compile time, so cargo cannot fingerprint them on its own:
//! `rerun-if-env-changed` is what makes a flipped flag produce a new kernel instead of
//! silently booting the cached one.

/// The faults `GREY_OS_EXCEPTION_TEST` accepts: the value, and the cfg it sets.
///
/// `guard` is M3's: the heap's guard page is unmapped, so writing one byte past the end of the
/// heap is a real page fault raised by hardware rather than an instruction chosen to fault.
const FAULTS: [(&str, &str); 5] = [
    ("gp", "grey_os_fault_gp"),
    ("pf", "grey_os_fault_pf"),
    ("ud", "grey_os_fault_ud"),
    ("df", "grey_os_fault_df"),
    ("guard", "grey_os_fault_guard"),
];

fn main() {
    // Set, unset and changed all have to rebuild, because both flags live in the output.
    println!("cargo::rerun-if-env-changed=GREY_OS_PANIC_TEST");
    println!("cargo::rerun-if-env-changed=GREY_OS_EXCEPTION_TEST");
    println!("cargo::rerun-if-env-changed=GREY_OS_INPUT_TEST");
    println!("cargo::rerun-if-env-changed=GREY_OS_INTERACTIVE");
    // Declares the cfgs so `--check-cfg` accepts them rather than warning about a cfg
    // nothing in this workspace is expected to set.
    println!("cargo::rustc-check-cfg=cfg(grey_os_panic_test)");
    println!("cargo::rustc-check-cfg=cfg(grey_os_input_test)");
    println!("cargo::rustc-check-cfg=cfg(grey_os_interactive)");
    for (_, cfg) in FAULTS {
        println!("cargo::rustc-check-cfg=cfg({cfg})");
    }

    // M6's acceptance run listens for keystrokes, so it has its own flag: a kernel with
    // nobody at the keyboard must not wait for one.
    let input = match std::env::var_os("GREY_OS_INPUT_TEST") {
        Some(value) => !value.is_empty() && value != "0",
        None => false,
    };
    if input {
        println!("cargo::rustc-cfg=grey_os_input_test");
    }

    // A preview has somebody at the keyboard by definition, so it compiles the shell in and
    // lets it run until `exit` (ADR-041). Separate from the input run, whose loop has to end
    // on its own because a harness is waiting for the process. `xtask` sets this for
    // `run --interactive` and clears it for everything else; `cargo run` passes whatever the
    // shell has exported.
    let interactive = match std::env::var_os("GREY_OS_INTERACTIVE") {
        Some(value) => !value.is_empty() && value != "0",
        None => false,
    };
    if interactive {
        println!("cargo::rustc-cfg=grey_os_interactive");
    }

    // Empty or `0` counts as off. `=1` is the documented spelling, but rejecting
    // `=yes` from a shell would only annoy the person typing it.
    let enabled = match std::env::var_os("GREY_OS_PANIC_TEST") {
        Some(value) => !value.is_empty() && value != "0",
        None => false,
    };
    if enabled {
        println!("cargo::rustc-cfg=grey_os_panic_test");
    }

    // A misspelt fault name has to be an error: silently building a normal kernel would make
    // the fault-path test pass by measuring nothing.
    let Some(fault) = std::env::var_os("GREY_OS_EXCEPTION_TEST") else {
        return;
    };
    let fault = fault.to_string_lossy();
    if fault.is_empty() || fault == "0" {
        return;
    }
    match FAULTS.iter().find(|(value, _)| *value == fault.as_ref()) {
        Some((_, cfg)) => println!("cargo::rustc-cfg={cfg}"),
        None => panic!("GREY_OS_EXCEPTION_TEST={fault} is not one of gp, pf, ud, df or guard"),
    }
}
