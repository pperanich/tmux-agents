use std::process::Command;

use tma_test_support::{tmux_available, wait_until, Scratch, POLL_CEILING};

#[test]
fn scratch_shells_do_not_load_user_configuration() {
    use std::os::unix::fs::PermissionsExt;

    if !tmux_available() {
        return;
    }
    // A subprocess gives this test hostile ambient variables without racing other tests.
    if std::env::var_os("TMA_TEST_SHELL_ISOLATION_CHILD").is_none() {
        let fixture = Scratch::new("shell_fixture");
        let user_home = fixture.workdir.join("fake user");
        let zim = user_home.join(".zim");
        std::fs::create_dir_all(&zim).unwrap();
        let cache = zim.join("init.zsh");
        std::fs::write(&cache, "original prompt\n").unwrap();
        let startup = "printf 'changed prompt\\n' > \"$TMA_TEST_USER_CACHE\"\n";
        for name in [".profile", ".bashrc", ".zshenv", ".zshrc", "env.sh"] {
            std::fs::write(user_home.join(name), startup).unwrap();
        }
        let shell = user_home.join("user-shell");
        std::fs::write(&shell, format!("#!/bin/sh\n{startup}exec /bin/sh \"$@\"\n")).unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(
            user_home.join(".tmux.conf"),
            "set -g default-shell /no/such/user-shell\n",
        )
        .unwrap();

        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "scratch_shells_do_not_load_user_configuration",
                "--nocapture",
            ])
            .env("TMA_TEST_SHELL_ISOLATION_CHILD", "1")
            .env("TMA_TEST_USER_CACHE", &cache)
            .env("HOME", &user_home)
            .env("SHELL", &shell)
            .env("XDG_CONFIG_HOME", user_home.join("redirected-config"))
            .env("XDG_CACHE_HOME", &user_home)
            .env("XDG_DATA_HOME", &user_home)
            .env("XDG_STATE_HOME", &user_home)
            .env("XDG_RUNTIME_DIR", &user_home)
            .env("ENV", user_home.join("env.sh"))
            .env("BASH_ENV", user_home.join("env.sh"))
            .env("ZDOTDIR", &user_home)
            .env("ZIM_HOME", &zim)
            .env("ZIM_CONFIG_FILE", user_home.join("zimrc"))
            .env("TMUX", "/no/such/socket,1,0")
            .env("TMUX_PANE", "%9999")
            .output()
            .unwrap();
        assert_eq!(std::fs::read_to_string(cache).unwrap(), "original prompt\n");
        assert!(
            out.status.success(),
            "isolated child failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        return;
    }

    let mut s = Scratch::new("shell_isolation");
    // Also exercise paths that cannot be interpolated into shell syntax unquoted.
    let workdir = s.workdir.with_extension("path with spaces");
    std::fs::rename(&s.workdir, &workdir).unwrap();
    s.workdir = workdir;
    let home = s.workdir.join("home");
    let report = "printf '%s\\n' \"$HOME\" \"$SHELL\" \"$XDG_CONFIG_HOME\" \
        \"$XDG_CACHE_HOME\" \"$XDG_DATA_HOME\" \"$XDG_STATE_HOME\" \"$XDG_RUNTIME_DIR\" \
        \"${ENV-unset}\" \"${BASH_ENV-unset}\" \"${ZDOTDIR-unset}\" \
        \"${ZIM_HOME-unset}\" \"${ZIM_CONFIG_FILE-unset}\" > \"$HOME/report\"";
    let expected = format!(
        "{home}\n/bin/sh\n{home}/.config\n{home}/.cache\n{home}/.local/share\n\
         {home}/.local/state\n{home}/.runtime\nunset\nunset\nunset\nunset\nunset\n",
        home = home.display()
    );
    let check_report = || {
        assert!(
            wait_until(POLL_CEILING, || {
                std::fs::read_to_string(home.join("report")).ok().as_deref() == Some(&expected)
            }),
            "pane did not inherit the isolated environment"
        );
        std::fs::remove_file(home.join("report")).unwrap();
    };

    // The default login shell and the outer shell for a single command both need isolation.
    assert!(s
        .tmux(&["new-session", "-d", "-s", "bare"])
        .status
        .success());
    assert_eq!(s.get("bare", "#{default-shell}"), "/bin/sh");
    assert!(s
        .tmux(&["send-keys", "-t", "bare", report, "Enter"])
        .status
        .success());
    check_report();
    assert!(s
        .tmux(&[
            "new-window",
            "-d",
            "-t",
            "bare",
            &format!("{report}; exec sleep 100000"),
        ])
        .status
        .success());
    check_report();

    let pane = s.new_shell_pane();
    assert!(s
        .tmux(&["send-keys", "-t", &pane, report, "Enter"])
        .status
        .success());
    check_report();
    assert_eq!(
        std::fs::metadata(home.join(".runtime"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
}
