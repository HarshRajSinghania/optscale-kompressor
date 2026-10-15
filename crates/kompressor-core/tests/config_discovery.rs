//! Where `Config::discover` looks, and in what order.
//!
//! An integration test rather than a unit one, and deliberately: the crate is
//! `#![forbid(unsafe_code)]`, and since edition 2024 `std::env::set_var` is
//! unsafe — it races any other thread reading the environment, which is why it
//! was made unsafe at all. `forbid` cannot be relaxed from inside the crate,
//! and relaxing it to `deny` to accommodate one test would weaken a guarantee
//! the library makes about itself. An integration test compiles as its own
//! crate and inherits none of that, so the lint stays absolute where it
//! matters and this test keeps setting the variables it has to set.
//!
//! The process environment is global, so this file must hold **one** test:
//! two running in parallel would fight over `KOMPRESSOR_CONFIG`.

use kompressor_core::Config;

#[test]
fn the_configuration_file_is_discovered_in_the_documented_order() {
    let scratch = std::env::temp_dir().join(format!("kompressor-discover-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(scratch.join("xdg/kompressor")).unwrap();
    std::fs::write(scratch.join("xdg/kompressor/config.toml"), "").unwrap();
    std::fs::write(scratch.join("kompressor.toml"), "").unwrap();

    let previous_cwd = std::env::current_dir().unwrap();
    // Single-threaded, and every variable is set before the code under test
    // reads it: the race `set_var` warns about cannot happen here.
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", scratch.join("xdg"));
        std::env::remove_var("KOMPRESSOR_CONFIG");
    }

    // A directory with no project file falls through to the XDG default.
    std::env::set_current_dir(&scratch).unwrap();
    std::fs::rename(scratch.join("kompressor.toml"), scratch.join("hidden.toml")).unwrap();
    assert_eq!(
        Config::discover(),
        Some(scratch.join("xdg/kompressor/config.toml")),
        "the per-user default is the last resort"
    );

    // A project file outranks it.
    std::fs::rename(scratch.join("hidden.toml"), scratch.join("kompressor.toml")).unwrap();
    assert_eq!(
        Config::discover(),
        Some(std::path::PathBuf::from("kompressor.toml")),
        "a file in the working directory belongs to the project"
    );

    // And an explicit name outranks everything, even when it does not exist: a
    // typo there has to be reported, not silently ignored.
    unsafe { std::env::set_var("KOMPRESSOR_CONFIG", "/nonexistent/typo.toml") };
    assert_eq!(
        Config::discover(),
        Some(std::path::PathBuf::from("/nonexistent/typo.toml"))
    );
    assert!(
        Config::load(None).is_err(),
        "and loading it reports the missing file rather than defaulting"
    );

    unsafe {
        std::env::remove_var("KOMPRESSOR_CONFIG");
        std::env::remove_var("XDG_CONFIG_HOME");
    }
    std::env::set_current_dir(previous_cwd).unwrap();
    let _ = std::fs::remove_dir_all(&scratch);
}
