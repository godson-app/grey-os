//! Turns `GREY_OS_PANIC_TEST` into `cfg(grey_os_panic_test)`.
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
//! ```
//!
//! The value is read at compile time, so cargo cannot fingerprint it on its own:
//! `rerun-if-env-changed` is what makes a flipped flag produce a new kernel instead of
//! silently booting the cached one.

fn main() {
    // Set, unset and changed all have to rebuild, because the flag lives in the output.
    println!("cargo::rerun-if-env-changed=GREY_OS_PANIC_TEST");
    // Declares the cfg so `--check-cfg` accepts it rather than warning about a cfg
    // nothing in this workspace is expected to set.
    println!("cargo::rustc-check-cfg=cfg(grey_os_panic_test)");

    // Empty or `0` counts as off. `=1` is the documented spelling, but rejecting
    // `=yes` from a shell would only annoy the person typing it.
    let enabled = match std::env::var_os("GREY_OS_PANIC_TEST") {
        Some(value) => !value.is_empty() && value != "0",
        None => false,
    };
    if enabled {
        println!("cargo::rustc-cfg=grey_os_panic_test");
    }
}
