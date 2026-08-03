use std::path::Path;
use std::process::{Command, Output};

#[test]
fn default_path_is_read_only_and_resolves_the_documented_macos_location() {
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_conversation-memory-probe"))
        .arg("default-path")
        .env("HOME", home.path())
        .output()
        .unwrap();

    let stdout = assert_success(output, &["status=ok", "command=default-path"]);
    assert!(stdout.contains("Library/Application Support/Conversation Runtime/runtime.sqlite3"));
    assert!(!home.path().join("Library").exists());
}

#[test]
fn supports_the_controlled_memory_lifecycle() {
    let fixture = tempfile::tempdir().unwrap();
    let database = fixture.path().join("memory.sqlite3");

    assert_success(
        run(&database, &["init"]),
        &["status=ok", "command=init", "schema_version=1"],
    );

    let added = assert_success(
        run(
            &database,
            &[
                "add",
                "--kind",
                "semantic",
                "--content",
                "Prefers jasmine tea",
                "--confidence",
                "900",
                "--at",
                "100",
            ],
        ),
        &[
            "command=add",
            "memory_id=1",
            "kind=semantic",
            "state=active",
            "revision=1",
        ],
    );
    assert!(!added.contains("database="));

    assert_success(
        run(&database, &["list", "--at", "100"]),
        &[
            "command=list",
            "count=1",
            "item.0.memory_id=1",
            "item.0.content=\"Prefers jasmine tea\"",
        ],
    );

    assert_success(
        run(&database, &["inspect", "1", "--at", "100"]),
        &[
            "command=inspect",
            "memory_id=1",
            "source_count=1",
            "source.0.kind=user_provided",
            "approval_count=0",
        ],
    );

    assert_success(
        run(
            &database,
            &[
                "edit",
                "1",
                "--revision",
                "1",
                "--content",
                "Prefers roasted jasmine tea",
                "--confidence",
                "950",
                "--at",
                "110",
            ],
        ),
        &[
            "command=edit",
            "memory_id=1",
            "revision=2",
            "content=\"Prefers roasted jasmine tea\"",
        ],
    );

    assert_success(
        run(&database, &["pin", "1", "--revision", "2", "--at", "120"]),
        &["command=pin", "memory_id=1", "pinned=true", "revision=3"],
    );

    assert_success(
        run(&database, &["unpin", "1", "--revision", "3", "--at", "125"]),
        &["command=unpin", "memory_id=1", "pinned=false", "revision=4"],
    );

    assert_success(
        run(&database, &["pin", "1", "--revision", "4", "--at", "126"]),
        &["command=pin", "memory_id=1", "pinned=true", "revision=5"],
    );

    assert_success(
        run(
            &database,
            &[
                "retrieve",
                "--turn",
                "7",
                "--query",
                "jasmine tea",
                "--at",
                "130",
            ],
        ),
        &[
            "command=retrieve",
            "trace_id=1",
            "turn_id=7",
            "count=1",
            "item.0.memory_id=1",
            "item.0.reason=pinned_match",
            "item.0.content=\"Prefers roasted jasmine tea\"",
        ],
    );

    assert_success(
        run(
            &database,
            &[
                "add",
                "--kind",
                "identity",
                "--content",
                "Uses the name River",
                "--at",
                "140",
            ],
        ),
        &[
            "memory_id=2",
            "kind=identity",
            "state=candidate",
            "revision=1",
        ],
    );

    assert_success(
        run(
            &database,
            &[
                "approve",
                "2",
                "--revision",
                "1",
                "--confirmation",
                "confirmation-2",
                "--actor",
                "operator",
                "--at",
                "150",
            ],
        ),
        &[
            "command=approve",
            "memory_id=2",
            "state=active",
            "revision=2",
            "approval.confirmation_id=\"confirmation-2\"",
        ],
    );

    assert_success(
        run(
            &database,
            &[
                "add",
                "--kind",
                "working",
                "--content",
                "Temporary topic",
                "--expires-at",
                "170",
                "--at",
                "160",
            ],
        ),
        &["memory_id=3", "kind=working", "state=active"],
    );

    assert_success(
        run(
            &database,
            &[
                "edit",
                "3",
                "--revision",
                "1",
                "--expires-at",
                "180",
                "--at",
                "165",
            ],
        ),
        &[
            "command=edit",
            "memory_id=3",
            "retention=working",
            "expires_at_ms=180",
            "revision=2",
        ],
    );

    assert_success(
        run(&database, &["expire", "--at", "180"]),
        &["command=expire", "expired_count=1"],
    );

    assert_success(
        run(&database, &["delete", "1", "--revision", "5"]),
        &["command=delete", "memory_id=1", "deleted=true"],
    );
    assert_success(
        run(&database, &["list", "--at", "180"]),
        &["command=list", "count=2"],
    );
}

#[test]
fn rejects_relative_database_paths_before_storage_access() {
    let output = Command::new(env!("CARGO_BIN_EXE_conversation-memory-probe"))
        .args(["--database", "relative.sqlite3", "init"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "status=error\nstage=arguments\nerror=database path must be absolute\n"
    );
}

#[test]
fn mutation_conflicts_do_not_echo_sensitive_content() {
    let fixture = tempfile::tempdir().unwrap();
    let database = fixture.path().join("memory.sqlite3");
    assert!(run(&database, &["init"]).status.success());
    assert!(run(
        &database,
        &[
            "add",
            "--kind",
            "semantic",
            "--content",
            "private phrase",
            "--at",
            "100",
        ],
    )
    .status
    .success());

    let output = run(
        &database,
        &[
            "edit",
            "1",
            "--revision",
            "99",
            "--content",
            "replacement secret",
            "--at",
            "110",
        ],
    );

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        stderr,
        "status=error\nstage=store\nerror_kind=conflict\nerror=memory record changed concurrently\n"
    );
    assert!(!stderr.contains("private phrase"));
    assert!(!stderr.contains("replacement secret"));
}

fn run(database: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_conversation-memory-probe"))
        .arg("--database")
        .arg(database)
        .args(arguments)
        .output()
        .unwrap()
}

fn assert_success(output: Output, expected_lines: &[&str]) -> String {
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    for line in expected_lines {
        assert!(
            stdout.lines().any(|actual| actual == *line),
            "missing {line:?} in {stdout:?}"
        );
    }
    stdout
}
