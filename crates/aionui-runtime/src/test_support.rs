pub(crate) fn run_in_env_child<F>(test_name: &str, configure: F) -> bool
where
    F: FnOnce(&mut std::process::Command),
{
    const CHILD_ENV: &str = "AIONUI_RUNTIME_ENV_TEST_CHILD";

    if std::env::var_os(CHILD_ENV).is_some() {
        return true;
    }

    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .arg("--exact")
        .arg(test_name)
        .arg("--nocapture")
        .env(CHILD_ENV, "1");
    configure(&mut command);
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "child test failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    false
}

/// Build a local, version-only runtime without consulting Node/npm on PATH.
pub(crate) fn fake_managed_runtime(root: &std::path::Path) -> crate::ResolvedNodeRuntime {
    let bin = if cfg!(windows) {
        root.to_path_buf()
    } else {
        root.join("bin")
    };
    std::fs::create_dir_all(&bin).expect("create runtime bin");
    let node = bin.join(if cfg!(windows) { "node.exe" } else { "node" });
    let npm = bin.join(if cfg!(windows) { "npm.cmd" } else { "npm" });
    let npx = bin.join(if cfg!(windows) { "npx.cmd" } else { "npx" });

    #[cfg(windows)]
    {
        // A shebang file named node.exe is not executable on Windows. Compile
        // this tiny stand-in with the same local toolchain that runs the tests.
        let source = root.join("fake_node.rs");
        std::fs::write(
            &source,
            r#"fn main() {
    if std::env::args().nth(1).as_deref() != Some("--version") {
        std::process::exit(2);
    }
    println!("v24.11.0");
}
"#,
        )
        .expect("write fake node source");
        let output = std::process::Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
            .arg("--edition=2024")
            .arg(&source)
            .arg("-o")
            .arg(&node)
            .output()
            .expect("compile fake node");
        assert!(
            output.status.success(),
            "fake node compilation failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        for path in [&npm, &npx] {
            std::fs::write(
                path,
                "@echo off\r\nif not \"%~1\"==\"--version\" exit /b 2\r\necho 24.11.0\r\n",
            )
            .expect("write version shim");
        }
    }
    #[cfg(not(windows))]
    {
        std::fs::write(&node, "#!/bin/sh\necho v24.11.0\n").expect("write node");
        for path in [&npm, &npx] {
            std::fs::write(path, "#!/bin/sh\necho 24.11.0\n").expect("write version script");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [&node, &npm, &npx] {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod version script");
            }
        }
    }

    crate::ResolvedNodeRuntime {
        source: crate::ResolvedNodeSource::Managed,
        root: root.to_path_buf(),
        version: semver::Version::new(0, 0, 0),
        node_path: node,
        npm_path: npm,
        npm_args_prefix: vec![],
        npx_path: npx,
        npx_args_prefix: vec![],
        env: vec![],
    }
}
