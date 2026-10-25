//! `oauth:SITE` inputs through the command line, without network access.
mod common;
use std::process::{Command, Stdio};

fn gallery_dl() -> Command {
    let mut command = common::binary_command();
    command
        .args(["--native-config-ignore", "-o", "cache.file=null"])
        .args(["-o", "extractor.oauth.browser=false"])
        .stdin(Stdio::null());
    command
}

#[test]
fn pixiv_prints_the_login_url_and_requires_interactive_input() {
    let output = gallery_dl().arg("oauth:pixiv").output().unwrap();
    assert!(!output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stdout.contains("\nhttps://app-api.pixiv.net/web/v1/login?code_challenge="),
        "{stdout}"
    );
    assert!(stdout.contains("&code_challenge_method=S256&client=pixiv-android\n"));
    assert!(
        stderr.contains("Please open this URL in your browser:"),
        "{stderr}"
    );
    assert!(stderr.contains("User input required (code)"), "{stderr}");
}

#[test]
fn unknown_sites_and_invalid_settings_are_rejected() {
    let output = gallery_dl().arg("oauth:twitter").output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("unsupported oauth: input"), "{stderr}");

    let output = gallery_dl()
        .args(["-o", "extractor.oauth.port=0", "oauth:reddit"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("oauth port must be 1-65535"), "{stderr}");
}

#[test]
fn listed_as_an_extractor_category() {
    let output = common::binary_command()
        .arg("--list-extractors")
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout
            .lines()
            .any(|line| line
                == "oauth: deviantart, flickr, mastodon, pixiv, reddit, smugmug, tumblr"),
        "{stdout}"
    );
}
