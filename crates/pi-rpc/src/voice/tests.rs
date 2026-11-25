use super::*;

// Explicit opt-in: exercised using an absolute, already installed Node path.
// No package, device, user config, worker native code, or downloads are used.
fn decoy(script: &str) -> Result<VoiceEvent> {
    let node = PathBuf::from(std::env::var_os("VOICE_TEST_NODE").expect("set VOICE_TEST_NODE"));
    ensure!(node.is_absolute(), "VOICE_TEST_NODE must be absolute");
    let mut command = crate::command(&node);
    command.current_dir(node.parent().unwrap()).env_clear();
    if let Some(root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", root);
    }
    command
        .args(["-e", script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut tree = Tree::spawn(&mut command)?;
    supervise(
        &mut tree,
        true,
        &Controls::default(),
        &async_channel::bounded(3).0,
    )
}

#[test]
#[ignore = "requires an absolute VOICE_TEST_NODE"]
fn rejects_trailing_helper_output() {
    for trailing in [r#"{"type":"ready"}\n"#, "partial", "\\n"] {
        let script = format!(
            r#"process.stdin.once('data', () => {{
            process.stdin.pause();
            process.stdout.write('{{"type":"ready"}}\n{}');
        }});"#,
            trailing
        );
        let error = decoy(&script).unwrap_err();
        assert_eq!(error.to_string(), "Trailing voice helper output");
    }
}

#[test]
#[ignore = "requires an absolute VOICE_TEST_NODE"]
fn helper_exit_terminates_grandchild_holding_pipe() {
    let event = decoy(r#"process.stdin.once('data', () => {
        process.stdin.pause();
        const child = require('node:child_process').spawn(process.execPath,
            ['-e', 'setInterval(() => console.log("unexpected survivor"), 10000)'],
            { stdio: ['ignore', 'inherit', 'ignore'], windowsHide: true });
        child.once('spawn', () => process.stdout.write('{"type":"ready"}\n', () => process.exit(0)));
    });"#).unwrap();
    // The grandchild holds stdout open and never closes it on its own;
    // Ready is returned only after pipe EOF and the helper has been reaped.
    assert_eq!(event, VoiceEvent::Ready);
}

// A real bundled package and its pinned Node, in a disposable install with
// no voice assets or user config. Probe must not start audio or write a home.
#[test]
#[ignore = "requires VOICE_TEST_BUNDLE and its pinned VOICE_TEST_NODE"]
fn pinned_bundle_probes_without_opening_audio() {
    let absolute = |key| {
        let path = PathBuf::from(std::env::var_os(key).expect("set voice fixture path"));
        assert!(path.is_absolute());
        dunce::canonicalize(path).unwrap()
    };
    let root = absolute("VOICE_TEST_BUNDLE");
    let node = absolute("VOICE_TEST_NODE");
    let package = dunce::canonicalize(root.join("versions/4.9.0/node_modules/gentle-pi")).unwrap();
    let config = root.join("config");
    assert!(!config.exists(), "use a fixture without a config home");
    let env = std::env::var_os("SystemRoot")
        .map(|root| ("SystemRoot", root))
        .into_iter()
        .collect();
    let adapter = VoiceAdapter {
        root: root.clone(),
        package,
        home: root,
        node,
        mode: "bundled",
        setup_anchor: true,
        env,
    };
    assert_eq!(
        adapter.probe().unwrap(),
        VoiceEvent::Error {
            code: "not-installed".into()
        }
    );
    assert!(!config.exists());
}

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scratch() -> Scratch {
    let name = format!(
        "pi-voice-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let root = std::env::temp_dir().join(name);
    std::fs::create_dir(&root).unwrap();
    Scratch(dunce::canonicalize(root).unwrap())
}

#[cfg(target_os = "linux")]
#[test]
fn setup_publication_never_replaces_an_existing_empty_component() {
    use std::{fs, os::unix::fs::PermissionsExt as _};

    let scratch = scratch();
    let home = &scratch.0;
    fs::set_permissions(home, fs::Permissions::from_mode(0o700)).unwrap();
    let workspace = ".desktop-setup-A1B2C3";
    let source = home.join(workspace).join("runtime/pinned");
    let destination = home.join("runtime/pinned");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join(".gentle-voice.json"), "verified").unwrap();
    fs::create_dir_all(&destination).unwrap();

    let publish = |workspace, id, current| {
        setup::publish(home, workspace, VoiceSetupComponent::Runtime, id, || {
            current
        })
    };
    let publish_pinned = || publish(workspace, "pinned", true);
    assert_eq!(publish_pinned().unwrap(), b"publish-conflict\n");
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
    assert_eq!(
        fs::read_to_string(source.join(".gentle-voice.json")).unwrap(),
        "verified"
    );
    assert!(publish(workspace, "../pinned", true).is_err());
    assert!(publish("../outside", "pinned", true).is_err());

    fs::remove_dir(&destination).unwrap();
    let parent = destination.parent().unwrap();
    fs::set_permissions(parent, fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(publish_pinned().unwrap(), b"unsafe-path\n");
    assert!(source.is_dir());
    assert!(!destination.exists());
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        publish(workspace, "pinned", false).unwrap(),
        b"backend-changed\n"
    );
    assert!(source.is_dir());
    assert!(!destination.exists());
    assert_eq!(publish_pinned().unwrap(), b"published\n");
    assert_eq!(
        fs::read_to_string(home.join("runtime/pinned/.gentle-voice.json")).unwrap(),
        "verified"
    );
    assert!(!source.exists());
}

#[test]
fn replaced_node_is_rejected_before_spawn() {
    use std::fs;
    let scratch = scratch();
    let node = scratch
        .0
        .join(if cfg!(windows) { "node.exe" } else { "node" });
    fs::write(&node, "original").unwrap();
    let adapter = VoiceAdapter {
        root: scratch.0.clone(),
        package: scratch.0.clone(),
        home: scratch.0.clone(),
        node: node.clone(),
        mode: "bundled",
        setup_anchor: true,
        env: vec![],
    };
    let replacement = scratch.0.join("replacement");
    fs::write(
        &replacement,
        "#!/bin/sh\nprintf '%s\\n' '{\"type\":\"ready\"}'\n: > executed\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&replacement, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fs::remove_file(&node).unwrap();
    fs::rename(replacement, node).unwrap();
    assert_eq!(
        adapter.probe().unwrap(),
        VoiceEvent::Error {
            code: "unsupported".into()
        }
    );
    assert!(!scratch.0.join("executed").exists());
}

#[test]
fn voice_pairing_rejects_a_retargeted_bundle() {
    use std::fs;

    let scratch = scratch();
    let prefix = &scratch.0;
    let launcher = prefix.join("bin/gentle-shell");
    let runtime = prefix.join("runtime/node-24/bin");
    let current = prefix.join("current");
    let original = prefix.join("versions/a/node_modules/gentle-pi");
    let replacement = prefix.join("versions/b/node_modules/gentle-pi");
    for path in [
        &original,
        &replacement,
        launcher.parent().unwrap(),
        &runtime,
    ] {
        fs::create_dir_all(path).unwrap();
    }
    fs::write(runtime.join("node"), "not executed").unwrap();
    fs::write(
        &launcher,
        "# gentle-shell bundled launcher (generated; do not edit)\nruntime/node-24/bin/node\n",
    )
    .unwrap();
    let select = |version: &str| {
        #[cfg(unix)]
        {
            if current.exists() {
                fs::remove_file(&current).unwrap();
            }
            std::os::unix::fs::symlink(format!("versions/{version}"), &current).unwrap();
        }
        #[cfg(not(unix))]
        fs::write(&current, version).unwrap();
    };
    let launch = || LaunchSpec {
        program: launcher.clone(),
        args: vec![],
        cwd: prefix.clone(),
        env: vec![],
        bundled: crate::launch::bundled_install(&launcher),
        launcher: launcher.clone(),
    };
    select("a");
    let spec = launch();
    let pinned = VoiceAdapter::from_launch(&spec).unwrap();
    assert_eq!(pinned.package, original);
    assert!(pinned.matches_launch(&spec));

    select("b");
    assert!(!pinned.matches_launch(&spec));
    #[cfg(target_os = "linux")]
    {
        let (_session, events) = pinned
            .start_setup_until_shutdown(&spec, Arc::new(AtomicBool::new(false)))
            .unwrap();
        assert_eq!(
            events.recv_blocking().unwrap(),
            VoiceSetupEvent::Error {
                code: "backend-changed".into()
            }
        );
        assert!(!prefix.join("config").exists());
    }
    let restarted = launch();
    let next = VoiceAdapter::from_launch(&restarted).unwrap();
    assert_eq!(next.package, replacement);
    assert!(next.matches_launch(&restarted));
    assert!(!pinned.matches_launch(&restarted));
}

#[test]
fn bundled_pnpm_pairing_stays_in_the_selected_release_and_store() {
    use std::fs;

    fn link(target: &Path, path: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, path).unwrap();
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            let result = Command::new("cmd.exe")
                .args(["/D", "/V:OFF", "/C"])
                .raw_arg(r#"mklink /J "%VOICE_LINK%" "%VOICE_TARGET%""#)
                .env("VOICE_LINK", path)
                .env("VOICE_TARGET", target)
                .output()
                .unwrap();
            assert!(result.status.success(), "{result:?}");
        }
    }

    fn unlink(path: &Path) {
        #[cfg(unix)]
        fs::remove_file(path).unwrap();
        #[cfg(windows)]
        fs::remove_dir(path).unwrap();
    }

    let scratch = scratch();
    let prefix = scratch.0.join("bundle");
    let launcher = prefix.join("bin/gentle-shell");
    let runtime = prefix.join("runtime/node-24/bin");
    let release = prefix.join("versions/5.0.0-1.6.0");
    let modules = release.join("node_modules");
    let alias = modules.join("gentle-pi");
    let store = modules.join(".pnpm");
    let slot = "gentle-pi@5.0.0_@earendil-works+pi-coding-agent@0.1.0";
    let suffix = "node_modules/gentle-pi";
    let package = store.join(slot).join(suffix);
    for path in [&package, launcher.parent().unwrap(), &runtime] {
        fs::create_dir_all(path).unwrap();
    }
    fs::write(runtime.join("node"), "not executed").unwrap();
    fs::write(
        &launcher,
        "# gentle-shell bundled launcher (generated; do not edit)\nruntime/node-24/bin/node\n",
    )
    .unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("versions/5.0.0-1.6.0", prefix.join("current")).unwrap();
    #[cfg(windows)]
    fs::write(prefix.join("current"), "5.0.0-1.6.0\n").unwrap();
    #[cfg(windows)]
    link(&package, &alias);
    #[cfg(unix)]
    link(package.strip_prefix(&modules).unwrap(), &alias);
    let spec = LaunchSpec {
        program: launcher.clone(),
        args: vec![],
        cwd: prefix.clone(),
        env: vec![],
        bundled: crate::launch::bundled_install(&launcher),
        launcher,
    };
    let pinned = VoiceAdapter::from_launch(&spec).expect("confined pnpm package");
    assert_eq!(pinned.package, package);
    assert!(pinned.matches_launch(&spec));
    unlink(&alias);

    for target in [
        scratch.0.join(slot).join(suffix),
        prefix
            .join("versions/5.0.0-other/node_modules/.pnpm")
            .join(slot)
            .join(suffix),
        prefix.join("versions/4.9.0").join(suffix),
        store.join("gentle-pi@5.0.01_peer").join(suffix),
        store.join("gentle-pi@5.0.0_").join(suffix),
    ] {
        fs::create_dir_all(&target).unwrap();
        link(&target, &alias);
        assert!(VoiceAdapter::from_launch(&spec).is_none(), "{target:?}");
        assert!(!pinned.matches_launch(&spec), "{target:?}");
        unlink(&alias);
    }

    let redirected = modules.join("other-store");
    fs::rename(&store, &redirected).unwrap();
    link(&redirected, &store);
    link(&package, &alias);
    assert_eq!(
        dunce::canonicalize(&alias).unwrap(),
        redirected.join(slot).join(suffix)
    );
    assert!(VoiceAdapter::from_launch(&spec).is_none());
    assert!(!pinned.matches_launch(&spec));
    unlink(&alias);
    unlink(&store);
}

#[cfg(unix)]
#[test]
fn rejects_bundled_node_symlink_outside_selected_install() {
    use std::fs;
    use std::os::unix::fs::symlink;
    let scratch = scratch();
    let prefix = scratch.0.join("install");
    let launcher = prefix.join("bin/gentle-shell");
    let node = prefix.join("runtime/node-24/bin/node");
    let version = prefix.join("versions/4.9.0");
    fs::create_dir_all(version.join("node_modules/gentle-pi")).unwrap();
    fs::create_dir_all(node.parent().unwrap()).unwrap();
    fs::create_dir_all(launcher.parent().unwrap()).unwrap();
    fs::write(
        &launcher,
        "# gentle-shell bundled launcher (generated; do not edit)\nruntime/node-24/bin/node\n",
    )
    .unwrap();
    symlink("versions/4.9.0", prefix.join("current")).unwrap();
    let outside = scratch.0.join("outside");
    fs::write(&outside, "not executed").unwrap();
    symlink(&outside, &node).unwrap();
    let spec = LaunchSpec {
        program: launcher.clone(),
        args: vec![],
        cwd: prefix,
        env: vec![],
        bundled: crate::launch::bundled_install(&launcher),
        launcher,
    };
    assert!(spec.bundled.is_some());
    assert!(VoiceAdapter::from_launch(&spec).is_none());
    fs::remove_file(&node).unwrap();
    fs::write(&node, "not executed").unwrap();
    assert!(VoiceAdapter::from_launch(&spec).is_some());
}
