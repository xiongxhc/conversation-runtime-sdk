use std::collections::{HashMap, HashSet};
use std::env;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use conversation_memory::{
    MemoryStore, MemoryStoreError, MemoryStoreErrorKind, NeverCancelled, SqliteMemoryStore,
    SCHEMA_VERSION,
};
use conversation_protocol::{
    MemoryApproval, MemoryConfidence, MemoryDraft, MemoryId, MemoryKind, MemoryPatch,
    MemoryProvenance, MemoryProvenanceKind, MemoryRecord, MemoryRetention, MemoryRetrievalRequest,
    RuntimeError, SessionId, TurnId, UnixTimestampMillis,
};

const USAGE: &str = "usage: conversation-memory-probe default-path | --database <absolute-path> <init|add|list|inspect|edit|pin|unpin|approve|expire|delete|retrieve> [options]";

fn main() {
    match parse_invocation(env::args()).and_then(run) {
        Ok(output) => print!("{output}"),
        Err(error) => {
            eprint!("{}", error.report());
            process::exit(1);
        }
    }
}

struct Invocation {
    database: Option<PathBuf>,
    command: String,
    arguments: Vec<String>,
}

fn parse_invocation(arguments: impl IntoIterator<Item = String>) -> ProbeResult<Invocation> {
    let mut arguments = arguments.into_iter();
    let _program = arguments.next();
    let first = arguments
        .next()
        .ok_or_else(|| ProbeError::arguments(USAGE))?;
    if first == "default-path" {
        if arguments.next().is_some() {
            return Err(ProbeError::arguments(
                "default-path does not accept arguments",
            ));
        }
        return Ok(Invocation {
            database: None,
            command: first,
            arguments: Vec::new(),
        });
    }
    if first != "--database" {
        return Err(ProbeError::arguments(USAGE));
    }
    let database = PathBuf::from(
        arguments
            .next()
            .ok_or_else(|| ProbeError::arguments(USAGE))?,
    );
    if !database.is_absolute() {
        return Err(ProbeError::arguments("database path must be absolute"));
    }
    let command = arguments
        .next()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ProbeError::arguments(USAGE))?;
    Ok(Invocation {
        database: Some(database),
        command,
        arguments: arguments.collect(),
    })
}

fn run(invocation: Invocation) -> ProbeResult<String> {
    if invocation.command == "default-path" {
        return command_default_path();
    }
    let database = invocation
        .database
        .as_deref()
        .ok_or_else(|| ProbeError::arguments(USAGE))?;
    match invocation.command.as_str() {
        "init" => command_init(database, invocation.arguments),
        "add" => command_add(database, invocation.arguments),
        "list" => command_list(database, invocation.arguments),
        "inspect" => command_inspect(database, invocation.arguments),
        "edit" => command_edit(database, invocation.arguments),
        "pin" => command_pin(database, invocation.arguments, true),
        "unpin" => command_pin(database, invocation.arguments, false),
        "approve" => command_approve(database, invocation.arguments),
        "expire" => command_expire(database, invocation.arguments),
        "delete" => command_delete(database, invocation.arguments),
        "retrieve" => command_retrieve(database, invocation.arguments),
        _ => Err(ProbeError::arguments(USAGE)),
    }
}

fn command_default_path() -> ProbeResult<String> {
    let home = env::var_os("HOME").ok_or_else(|| ProbeError::system("HOME is not configured"))?;
    let home = PathBuf::from(home);
    if !home.is_absolute() {
        return Err(ProbeError::system("HOME must be an absolute path"));
    }
    let path = home
        .join("Library")
        .join("Application Support")
        .join("Conversation Runtime")
        .join("runtime.sqlite3");
    Ok(format!(
        "status=ok\ncommand=default-path\ndatabase_path={}\n",
        json(&path.to_string_lossy())
    ))
}

fn command_init(database: &Path, arguments: Vec<String>) -> ProbeResult<String> {
    require_no_arguments(&arguments)?;
    SqliteMemoryStore::initialize(database)?;
    Ok(format!(
        "status=ok\ncommand=init\nschema_version={SCHEMA_VERSION}\n"
    ))
}

fn command_add(database: &Path, arguments: Vec<String>) -> ProbeResult<String> {
    let options = Options::parse(arguments, &[])?;
    options.require_positionals(0)?;
    options.require_allowed(&[
        "kind",
        "content",
        "confidence",
        "at",
        "expires-at",
        "session",
        "retention",
        "source-id",
        "actor",
    ])?;
    let kind = parse_kind(options.required("kind")?)?;
    let content = options.required("content")?;
    let confidence = parse_confidence(options.optional("confidence")?.unwrap_or("1000"))?;
    let at = parse_timestamp_option(&options, "at")?;
    let retention =
        parse_retention(&options, kind, false)?.unwrap_or(MemoryRetention::UntilDeleted);
    if kind == MemoryKind::Working && !matches!(retention, MemoryRetention::Working { .. }) {
        return Err(ProbeError::arguments(
            "working memory requires --expires-at",
        ));
    }
    let provenance = MemoryProvenance::new(
        MemoryProvenanceKind::UserProvided,
        options.optional("source-id")?.unwrap_or("memory-probe:add"),
        at,
        options.optional("actor")?.unwrap_or("local-user"),
        None,
    )?;
    let draft = MemoryDraft::new(kind, content, provenance, confidence, at, retention)?;
    let record = open_store(database)?.create(draft)?;
    Ok(record_report("add", &record, ""))
}

fn command_list(database: &Path, arguments: Vec<String>) -> ProbeResult<String> {
    let options = Options::parse(arguments, &[])?;
    options.require_positionals(0)?;
    options.require_allowed(&["at"])?;
    let records = open_store(database)?.list(parse_timestamp_option(&options, "at")?)?;
    let mut output = String::from("status=ok\ncommand=list\n");
    writeln!(output, "count={}", records.len()).unwrap();
    for (index, record) in records.iter().enumerate() {
        write_record(&mut output, &format!("item.{index}."), record);
    }
    Ok(output)
}

fn command_inspect(database: &Path, arguments: Vec<String>) -> ProbeResult<String> {
    let options = Options::parse(arguments, &[])?;
    options.require_positionals(1)?;
    options.require_allowed(&["at"])?;
    let memory_id = parse_memory_id(&options.positionals[0])?;
    let inspection = open_store(database)?
        .inspect_with_sources(memory_id, parse_timestamp_option(&options, "at")?)?;
    let mut output = String::from("status=ok\ncommand=inspect\n");
    write_record(&mut output, "", inspection.record());
    writeln!(output, "source_count={}", inspection.sources().len()).unwrap();
    for (index, source) in inspection.sources().iter().enumerate() {
        writeln!(output, "source.{index}.kind={}", source.kind().as_str()).unwrap();
        writeln!(
            output,
            "source.{index}.source_id={}",
            json(source.source_id())
        )
        .unwrap();
        writeln!(
            output,
            "source.{index}.source_timestamp_ms={}",
            source.source_timestamp().get()
        )
        .unwrap();
        writeln!(output, "source.{index}.actor={}", json(source.actor())).unwrap();
        writeln!(
            output,
            "source.{index}.content_digest={}",
            source
                .content_digest()
                .map(json)
                .unwrap_or_else(|| "null".into())
        )
        .unwrap();
    }
    writeln!(output, "approval_count={}", inspection.approvals().len()).unwrap();
    for (index, approval) in inspection.approvals().iter().enumerate() {
        writeln!(
            output,
            "approval.{index}.confirmation_id={}",
            json(approval.confirmation_id())
        )
        .unwrap();
        writeln!(output, "approval.{index}.actor={}", json(approval.actor())).unwrap();
        writeln!(
            output,
            "approval.{index}.confirmed_at_ms={}",
            approval.confirmed_at().get()
        )
        .unwrap();
        writeln!(
            output,
            "approval.{index}.approved_revision={}",
            approval.approved_revision()
        )
        .unwrap();
        writeln!(
            output,
            "approval.{index}.content_digest={}",
            json(approval.content_digest())
        )
        .unwrap();
    }
    Ok(output)
}

fn command_edit(database: &Path, arguments: Vec<String>) -> ProbeResult<String> {
    let options = Options::parse(arguments, &[])?;
    options.require_positionals(1)?;
    options.require_allowed(&[
        "revision",
        "content",
        "confidence",
        "at",
        "expires-at",
        "session",
        "retention",
        "source-id",
        "actor",
    ])?;
    let memory_id = parse_memory_id(&options.positionals[0])?;
    let expected_revision = parse_revision(options.required("revision")?)?;
    let at = parse_timestamp_option(&options, "at")?;
    let store = open_store(database)?;
    let current = store.inspect(memory_id, at)?;
    let content = options.optional("content")?.map(str::to_owned);
    let confidence = options
        .optional("confidence")?
        .map(parse_confidence)
        .transpose()?;
    let retention = parse_retention(&options, current.kind(), true)?;
    let provenance = MemoryProvenance::new(
        MemoryProvenanceKind::UserEdited,
        options
            .optional("source-id")?
            .unwrap_or("memory-probe:edit"),
        at,
        options.optional("actor")?.unwrap_or("local-user"),
        None,
    )?;
    let patch = MemoryPatch::new(
        expected_revision,
        content,
        confidence,
        retention,
        at,
        provenance,
    )?;
    let record = store.edit(memory_id, patch)?;
    Ok(record_report("edit", &record, ""))
}

fn command_pin(database: &Path, arguments: Vec<String>, pinned: bool) -> ProbeResult<String> {
    let flag_names = if pinned { &["off"][..] } else { &[] };
    let options = Options::parse(arguments, flag_names)?;
    options.require_positionals(1)?;
    options.require_allowed(&["revision", "at"])?;
    let memory_id = parse_memory_id(&options.positionals[0])?;
    let pinned = pinned && !options.flags.contains("off");
    let record = open_store(database)?.set_pinned(
        memory_id,
        parse_revision(options.required("revision")?)?,
        pinned,
        parse_timestamp_option(&options, "at")?,
    )?;
    Ok(record_report(
        if pinned { "pin" } else { "unpin" },
        &record,
        "",
    ))
}

fn command_approve(database: &Path, arguments: Vec<String>) -> ProbeResult<String> {
    let options = Options::parse(arguments, &[])?;
    options.require_positionals(1)?;
    options.require_allowed(&["revision", "confirmation", "actor", "at"])?;
    let memory_id = parse_memory_id(&options.positionals[0])?;
    let approval = MemoryApproval::new(
        options.required("confirmation")?,
        options.optional("actor")?.unwrap_or("local-user"),
        parse_timestamp_option(&options, "at")?,
        parse_revision(options.required("revision")?)?,
    )?;
    let record = open_store(database)?.approve(memory_id, approval)?;
    Ok(record_report("approve", &record, ""))
}

fn command_expire(database: &Path, arguments: Vec<String>) -> ProbeResult<String> {
    let options = Options::parse(arguments, &[])?;
    options.require_positionals(0)?;
    options.require_allowed(&["session", "at"])?;
    let at = parse_timestamp_option(&options, "at")?;
    let store = open_store(database)?;
    let expired_count = if let Some(session) = options.optional("session")? {
        store.expire_session(
            SessionId::new(parse_positive_u64(session, "session identifier")?),
            at,
        )?
    } else {
        store.prune_expired(at)?
    };
    Ok(format!(
        "status=ok\ncommand=expire\nexpired_count={expired_count}\n"
    ))
}

fn command_delete(database: &Path, arguments: Vec<String>) -> ProbeResult<String> {
    let options = Options::parse(arguments, &[])?;
    options.require_positionals(1)?;
    options.require_allowed(&["revision"])?;
    let memory_id = parse_memory_id(&options.positionals[0])?;
    open_store(database)?.delete(memory_id, parse_revision(options.required("revision")?)?)?;
    Ok(format!(
        "status=ok\ncommand=delete\nmemory_id={}\ndeleted=true\n",
        memory_id.get()
    ))
}

fn command_retrieve(database: &Path, arguments: Vec<String>) -> ProbeResult<String> {
    let options = Options::parse(arguments, &[])?;
    options.require_positionals(0)?;
    options.require_allowed(&["turn", "query", "at", "maximum-items", "maximum-bytes"])?;
    let turn_id = TurnId::new(parse_positive_u64(
        options.required("turn")?,
        "turn identifier",
    )?);
    let request = MemoryRetrievalRequest::new(
        turn_id,
        options.required("query")?,
        parse_timestamp_option(&options, "at")?,
        parse_usize(
            options.optional("maximum-items")?.unwrap_or("4"),
            "maximum item count",
        )?,
        parse_usize(
            options.optional("maximum-bytes")?.unwrap_or("4096"),
            "maximum byte count",
        )?,
    )?;
    let retrieval = open_store(database)?.retrieve(request, &NeverCancelled)?;
    let trace = retrieval.trace();
    let mut output = String::from("status=ok\ncommand=retrieve\n");
    writeln!(output, "trace_id={}", trace.trace_id().get()).unwrap();
    writeln!(output, "turn_id={}", trace.turn_id().get()).unwrap();
    writeln!(output, "count={}", retrieval.items().len()).unwrap();
    writeln!(output, "used_bytes={}", trace.used_bytes()).unwrap();
    for (index, item) in retrieval.items().iter().enumerate() {
        writeln!(output, "item.{index}.memory_id={}", item.memory_id().get()).unwrap();
        writeln!(output, "item.{index}.kind={}", item.kind().as_str()).unwrap();
        writeln!(output, "item.{index}.reason={}", item.reason().as_str()).unwrap();
        writeln!(output, "item.{index}.content={}", json(item.content())).unwrap();
    }
    let exclusions = trace.exclusions();
    writeln!(output, "excluded.by_state={}", exclusions.by_state()).unwrap();
    writeln!(output, "excluded.by_expiry={}", exclusions.by_expiry()).unwrap();
    writeln!(
        output,
        "excluded.by_relevance={}",
        exclusions.by_relevance()
    )
    .unwrap();
    writeln!(
        output,
        "excluded.by_item_limit={}",
        exclusions.by_item_limit()
    )
    .unwrap();
    writeln!(
        output,
        "excluded.by_byte_limit={}",
        exclusions.by_byte_limit()
    )
    .unwrap();
    Ok(output)
}

fn parse_kind(value: &str) -> ProbeResult<MemoryKind> {
    match value {
        "working" => Ok(MemoryKind::Working),
        "episodic" => Ok(MemoryKind::Episodic),
        "semantic" => Ok(MemoryKind::Semantic),
        "identity" => Ok(MemoryKind::Identity),
        "relationship" => Ok(MemoryKind::Relationship),
        _ => Err(ProbeError::arguments("memory kind is invalid")),
    }
}

fn parse_retention(
    options: &Options,
    kind: MemoryKind,
    optional: bool,
) -> ProbeResult<Option<MemoryRetention>> {
    let expires_at = options
        .optional("expires-at")?
        .map(parse_timestamp)
        .transpose()?;
    let session = options
        .optional("session")?
        .map(|value| parse_positive_u64(value, "session identifier"))
        .transpose()?;
    let explicit = options.optional("retention")?;
    let specified = usize::from(expires_at.is_some())
        + usize::from(session.is_some())
        + usize::from(explicit.is_some());
    if specified > 1 {
        return Err(ProbeError::arguments(
            "memory retention options are mutually exclusive",
        ));
    }
    if let Some(expires_at) = expires_at {
        return Ok(Some(if kind == MemoryKind::Working {
            MemoryRetention::working(expires_at)
        } else {
            MemoryRetention::until(expires_at)
        }));
    }
    if let Some(session) = session {
        return Ok(Some(MemoryRetention::session(SessionId::new(session))));
    }
    if let Some(value) = explicit {
        return match value {
            "until-deleted" => Ok(Some(MemoryRetention::UntilDeleted)),
            _ => Err(ProbeError::arguments("memory retention is invalid")),
        };
    }
    if optional || kind == MemoryKind::Working {
        Ok(None)
    } else {
        Ok(Some(MemoryRetention::UntilDeleted))
    }
}

fn parse_timestamp_option(options: &Options, name: &str) -> ProbeResult<UnixTimestampMillis> {
    options
        .optional(name)?
        .map(parse_timestamp)
        .transpose()?
        .map(Ok)
        .unwrap_or_else(current_timestamp)
}

fn current_timestamp() -> ProbeResult<UnixTimestampMillis> {
    let milliseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ProbeError::system("system clock precedes Unix epoch"))?
        .as_millis();
    let milliseconds = i64::try_from(milliseconds)
        .map_err(|_| ProbeError::system("system clock exceeds timestamp range"))?;
    UnixTimestampMillis::new(milliseconds).map_err(ProbeError::from)
}

fn parse_timestamp(value: &str) -> ProbeResult<UnixTimestampMillis> {
    let value = value
        .parse::<i64>()
        .map_err(|_| ProbeError::arguments("timestamp must be a non-negative integer"))?;
    UnixTimestampMillis::new(value).map_err(ProbeError::from)
}

fn parse_memory_id(value: &str) -> ProbeResult<MemoryId> {
    MemoryId::new(parse_positive_u64(value, "memory identifier")?).map_err(ProbeError::from)
}

fn parse_revision(value: &str) -> ProbeResult<u64> {
    parse_positive_u64(value, "memory revision")
}

fn parse_confidence(value: &str) -> ProbeResult<MemoryConfidence> {
    let value = value
        .parse::<u16>()
        .map_err(|_| ProbeError::arguments("memory confidence must be 0 through 1000"))?;
    MemoryConfidence::new(value).map_err(ProbeError::from)
}

fn parse_positive_u64(value: &str, label: &'static str) -> ProbeResult<u64> {
    let value = value
        .parse::<u64>()
        .map_err(|_| ProbeError::arguments(label))?;
    if value == 0 {
        return Err(ProbeError::arguments(label));
    }
    Ok(value)
}

fn parse_usize(value: &str, label: &'static str) -> ProbeResult<usize> {
    value
        .parse::<usize>()
        .map_err(|_| ProbeError::arguments(label))
}

fn open_store(database: &Path) -> ProbeResult<SqliteMemoryStore> {
    SqliteMemoryStore::open(database).map_err(ProbeError::from)
}

fn require_no_arguments(arguments: &[String]) -> ProbeResult<()> {
    if arguments.is_empty() {
        Ok(())
    } else {
        Err(ProbeError::arguments("command does not accept arguments"))
    }
}

fn record_report(command: &str, record: &MemoryRecord, prefix: &str) -> String {
    let mut output = format!("status=ok\ncommand={command}\n");
    write_record(&mut output, prefix, record);
    output
}

fn write_record(output: &mut String, prefix: &str, record: &MemoryRecord) {
    writeln!(output, "{prefix}memory_id={}", record.id().get()).unwrap();
    writeln!(output, "{prefix}kind={}", record.kind().as_str()).unwrap();
    writeln!(output, "{prefix}state={}", record.state().as_str()).unwrap();
    writeln!(output, "{prefix}content={}", json(record.content())).unwrap();
    writeln!(output, "{prefix}confidence={}", record.confidence().get()).unwrap();
    writeln!(
        output,
        "{prefix}created_at_ms={}",
        record.created_at().get()
    )
    .unwrap();
    writeln!(
        output,
        "{prefix}updated_at_ms={}",
        record.updated_at().get()
    )
    .unwrap();
    writeln!(output, "{prefix}retention={}", record.retention().as_str()).unwrap();
    if let Some(expires_at) = record.retention().expires_at() {
        writeln!(output, "{prefix}expires_at_ms={}", expires_at.get()).unwrap();
    }
    if let Some(session_id) = record.retention().session_id() {
        writeln!(output, "{prefix}session_id={}", session_id.get()).unwrap();
    }
    writeln!(output, "{prefix}pinned={}", record.pinned()).unwrap();
    writeln!(output, "{prefix}revision={}", record.revision()).unwrap();
    if let Some(last_used_at) = record.last_used_at() {
        writeln!(output, "{prefix}last_used_at_ms={}", last_used_at.get()).unwrap();
    }
    if let Some(reason) = record.last_retrieval_reason() {
        writeln!(output, "{prefix}last_retrieval_reason={}", reason.as_str()).unwrap();
    }
    if let Some(approval) = record.approval() {
        writeln!(
            output,
            "{prefix}approval.confirmation_id={}",
            json(approval.confirmation_id())
        )
        .unwrap();
        writeln!(output, "{prefix}approval.actor={}", json(approval.actor())).unwrap();
        writeln!(
            output,
            "{prefix}approval.confirmed_at_ms={}",
            approval.confirmed_at().get()
        )
        .unwrap();
        writeln!(
            output,
            "{prefix}approval.approved_revision={}",
            approval.approved_revision()
        )
        .unwrap();
        writeln!(
            output,
            "{prefix}approval.content_digest={}",
            json(approval.content_digest())
        )
        .unwrap();
    }
}

fn json(value: &str) -> String {
    serde_json::to_string(value).expect("strings are JSON serializable")
}

struct Options {
    positionals: Vec<String>,
    values: HashMap<String, String>,
    flags: HashSet<String>,
}

impl Options {
    fn parse(arguments: Vec<String>, flag_names: &[&str]) -> ProbeResult<Self> {
        let flag_names = flag_names.iter().copied().collect::<HashSet<_>>();
        let mut positionals = Vec::new();
        let mut values = HashMap::new();
        let mut flags = HashSet::new();
        let mut arguments = arguments.into_iter();
        while let Some(argument) = arguments.next() {
            let Some(name) = argument.strip_prefix("--") else {
                positionals.push(argument);
                continue;
            };
            if name.is_empty() {
                return Err(ProbeError::arguments("option name must not be empty"));
            }
            if flag_names.contains(name) {
                if !flags.insert(name.to_owned()) {
                    return Err(ProbeError::arguments("option must not be repeated"));
                }
                continue;
            }
            let value = arguments
                .next()
                .ok_or_else(|| ProbeError::arguments("option requires a value"))?;
            if values.insert(name.to_owned(), value).is_some() {
                return Err(ProbeError::arguments("option must not be repeated"));
            }
        }
        Ok(Self {
            positionals,
            values,
            flags,
        })
    }

    fn require_positionals(&self, expected: usize) -> ProbeResult<()> {
        if self.positionals.len() == expected {
            Ok(())
        } else {
            Err(ProbeError::arguments(
                "command has an invalid number of positional arguments",
            ))
        }
    }

    fn require_allowed(&self, allowed: &[&str]) -> ProbeResult<()> {
        if self
            .values
            .keys()
            .all(|name| allowed.contains(&name.as_str()))
        {
            Ok(())
        } else {
            Err(ProbeError::arguments("command option is not recognized"))
        }
    }

    fn required(&self, name: &str) -> ProbeResult<&str> {
        self.values
            .get(name)
            .map(String::as_str)
            .ok_or_else(|| ProbeError::arguments("required command option is missing"))
    }

    fn optional(&self, name: &str) -> ProbeResult<Option<&str>> {
        Ok(self.values.get(name).map(String::as_str))
    }
}

type ProbeResult<T> = Result<T, ProbeError>;

enum ProbeError {
    Arguments(String),
    Store(MemoryStoreError),
    System(&'static str),
}

impl ProbeError {
    fn arguments(message: impl Into<String>) -> Self {
        Self::Arguments(message.into())
    }

    const fn system(message: &'static str) -> Self {
        Self::System(message)
    }

    fn report(&self) -> String {
        match self {
            Self::Arguments(message) => {
                format!("status=error\nstage=arguments\nerror={message}\n")
            }
            Self::Store(error) => format!(
                "status=error\nstage=store\nerror_kind={}\nerror={}\n",
                error_kind(error.kind()),
                store_message(error)
            ),
            Self::System(message) => {
                format!("status=error\nstage=system\nerror={message}\n")
            }
        }
    }
}

impl From<MemoryStoreError> for ProbeError {
    fn from(error: MemoryStoreError) -> Self {
        Self::Store(error)
    }
}

impl From<RuntimeError> for ProbeError {
    fn from(error: RuntimeError) -> Self {
        Self::Arguments(error.message().to_owned())
    }
}

fn error_kind(kind: MemoryStoreErrorKind) -> &'static str {
    match kind {
        MemoryStoreErrorKind::InvalidPath => "invalid_path",
        MemoryStoreErrorKind::NotInitialized => "not_initialized",
        MemoryStoreErrorKind::UnsupportedSchema => "unsupported_schema",
        MemoryStoreErrorKind::InvalidDatabase => "invalid_database",
        MemoryStoreErrorKind::NotFound => "not_found",
        MemoryStoreErrorKind::Conflict => "conflict",
        MemoryStoreErrorKind::Busy => "busy",
        MemoryStoreErrorKind::Cancelled => "cancelled",
        MemoryStoreErrorKind::LimitExceeded => "limit_exceeded",
        MemoryStoreErrorKind::Storage => "storage",
        _ => "unknown",
    }
}

fn store_message(error: &MemoryStoreError) -> String {
    match error.kind() {
        MemoryStoreErrorKind::Conflict => "memory record changed concurrently".into(),
        _ => error.to_string(),
    }
}
