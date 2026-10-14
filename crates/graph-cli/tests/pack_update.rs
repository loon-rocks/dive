mod support;

use support::Scratch;

#[test]
fn update_major_without_a_terminal_or_yes_refuses_on_stderr() {
    let scratch = Scratch::new();
    let config = scratch.read(".graph/config.toml");
    scratch
        .graph(&["pack", "update", "--major"])
        .code_is(1)
        .stdout_empty()
        .stderr_contains("there is no terminal to ask on; nothing was changed");
    assert_eq!(scratch.read(".graph/config.toml"), config);
    assert!(!scratch.exists(".graph/packs.lock"));
}

#[test]
fn yes_needs_major() {
    Scratch::new()
        .graph(&["pack", "update", "--yes"])
        .code_is(2)
        .stdout_empty()
        .stderr_contains("--major");
}
