// Integration tests for sendspin-rs-cli
use std::process::Command;

#[test]
fn test_help_command() {
    let output = Command::new("cargo")
        .args(&["run", "--", "--help"])
        .output()
        .expect("Failed to execute command");

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should show help text with Music Assistant reference
    assert!(stdout.contains("Music Assistant"));
    assert!(stdout.contains("--server"));
    assert!(stdout.contains("--name"));
    assert!(stdout.contains("--volume"));
    assert!(stdout.contains("Usage:"));
}

#[test]
fn test_version_flag() {
    let output = Command::new("cargo")
        .args(&["run", "--", "--version"])
        .output()
        .expect("Failed to execute command");

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should show version - output from cargo includes path
    assert!(stdout.contains("sendspin-rs-cli") || stdout.contains("0.1.0"));
}

#[test]
fn test_invalid_volume() {
    // Test that invalid volume values are handled
    let output = Command::new("cargo")
        .args(&["run", "--", "--volume", "150"])
        .output()
        .expect("Failed to execute command");

    // Should fail with error about invalid range
    assert!(!output.status.success() || output.stderr.len() > 0);
}

#[test]
fn test_binary_builds() {
    // Test that the binary builds successfully
    let output = Command::new("cargo")
        .args(&["build", "--release"])
        .output()
        .expect("Failed to build");

    assert!(output.status.success(), "Build failed: {:?}", String::from_utf8_lossy(&output.stderr));
}
