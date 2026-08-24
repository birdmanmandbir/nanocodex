//! Bounded saturation harness for the managed multiplayer-room transport.
//!
//! This is a library entry point for the `nanocodex2` consumer. It deliberately
//! keeps account credentials, invitations, membership cookies, and private
//! managed-agent identities out of both its configuration diagnostics and its
//! serializable result.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use futures_util::{StreamExt, stream::FuturesUnordered};
use serde::Serialize;
use tokio::{sync::mpsc, task::JoinHandle, time::Instant as TokioInstant};
use url::Url;

use super::room::{
    AccountKey, CreatedRoom, JoinedRoom, MessageId, PreparedRoomCreate, RoomApi, RoomConnection,
    RoomCursor, RoomError, RoomEventMessage, RoomEvents, RoomServerMessage, RoomTarget,
};

const MAX_ROOMS: usize = 8;
const MAX_GUESTS_PER_ROOM: usize = 15;
const MAX_MESSAGES_PER_GUEST: usize = 8;
const MAX_AGENT_PROMPTS_PER_ROOM: usize = 4;
const MIN_WALL_TIME: Duration = Duration::from_secs(30);
const MAX_WALL_TIME: Duration = Duration::from_secs(15 * 60);
const CLEANUP_RESERVE: Duration = Duration::from_secs(20);

/// Whether the harness stops after live fanout or reconnects every admitted
/// member at cursor zero and verifies the complete durable replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ConnectReplayBehavior {
    LiveOnly,
    ReconnectAllFromZero,
}

/// One bounded saturation run. `account_key` is intentionally neither
/// serializable nor debug-printable.
pub(crate) struct SaturationConfig {
    pub(crate) managed_url: Url,
    pub(crate) account_key: AccountKey,
    pub(crate) rooms: usize,
    pub(crate) guests_per_room: usize,
    pub(crate) messages_per_guest: usize,
    pub(crate) agent_prompts_per_room: usize,
    pub(crate) connect_replay: ConnectReplayBehavior,
    pub(crate) max_wall_time: Duration,
}

#[derive(Clone, Copy)]
struct WorkloadShape {
    rooms: usize,
    guests_per_room: usize,
    messages_per_guest: usize,
    agent_prompts_per_room: usize,
    connect_replay: ConnectReplayBehavior,
}

impl SaturationConfig {
    fn validate(&self) -> Result<(), SaturationError> {
        bounded("rooms", self.rooms, 1, MAX_ROOMS)?;
        bounded(
            "guests_per_room",
            self.guests_per_room,
            1,
            MAX_GUESTS_PER_ROOM,
        )?;
        bounded(
            "messages_per_guest",
            self.messages_per_guest,
            0,
            MAX_MESSAGES_PER_GUEST,
        )?;
        bounded(
            "agent_prompts_per_room",
            self.agent_prompts_per_room,
            0,
            MAX_AGENT_PROMPTS_PER_ROOM,
        )?;
        if !(MIN_WALL_TIME..=MAX_WALL_TIME).contains(&self.max_wall_time) {
            return Err(SaturationError::Configuration(format!(
                "max_wall_time must be {} through {} seconds",
                MIN_WALL_TIME.as_secs(),
                MAX_WALL_TIME.as_secs()
            )));
        }
        Ok(())
    }
}

fn bounded(
    name: &'static str,
    value: usize,
    minimum: usize,
    maximum: usize,
) -> Result<(), SaturationError> {
    if (minimum..=maximum).contains(&value) {
        Ok(())
    } else {
        Err(SaturationError::Configuration(format!(
            "{name} must be {minimum} through {maximum}"
        )))
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SaturationError {
    #[error("invalid saturation configuration: {0}")]
    Configuration(String),
    #[error(transparent)]
    Room(#[from] RoomError),
    #[error("saturation harness lost ownership of {0}")]
    Internal(&'static str),
}

/// JSON-safe output. Counts and latency samples never contain room/member,
/// agent/turn, invitation, cookie, or credential values.
#[derive(Debug, Serialize)]
pub(crate) struct SaturationSummary {
    pub(crate) managed_origin: String,
    pub(crate) requested: RequestedLoad,
    pub(crate) elapsed_ms: u64,
    pub(crate) timed_out: bool,
    pub(crate) operations: OperationSummaries,
    pub(crate) fanout: FanoutSummary,
    pub(crate) agent_outcomes: AgentOutcomeSummary,
    pub(crate) cleanup: CleanupSummary,
    pub(crate) failures: Vec<FailureSummary>,
    pub(crate) invariants: InvariantSummary,
}

impl SaturationSummary {
    pub(crate) fn passed(&self) -> bool {
        !self.timed_out && self.failures.is_empty() && self.invariants.violations == 0
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct RequestedLoad {
    rooms: usize,
    guests_per_room: usize,
    messages_per_guest: usize,
    agent_prompts_per_room: usize,
    connect_replay: ConnectReplayBehavior,
    max_wall_time_ms: u64,
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct OperationSummaries {
    create: OperationSummary,
    join: OperationSummary,
    connect: OperationSummary,
    send: OperationSummary,
    fanout: OperationSummary,
    agent_terminal: OperationSummary,
    replay: OperationSummary,
    cleanup: OperationSummary,
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct OperationSummary {
    attempted: u64,
    succeeded: u64,
    failed: u64,
    latency_us: Option<LatencyPercentiles>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct LatencyPercentiles {
    p50: u64,
    p95: u64,
    p99: u64,
    max: u64,
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct FanoutSummary {
    expected_live_deliveries: u64,
    observed_live_deliveries: u64,
    expected_agent_terminal_deliveries: u64,
    observed_agent_terminal_deliveries: u64,
    replay_clients_completed: u64,
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct AgentOutcomeSummary {
    messages: u64,
    cancelled: u64,
    failed: u64,
    blocked: u64,
    rate_limited: u64,
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct CleanupSummary {
    created_rooms: u64,
    settled_rooms: u64,
    pending_rooms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub(crate) struct FailureSummary {
    phase: &'static str,
    class: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    http_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
    count: u64,
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct InvariantSummary {
    checks: u64,
    violations: u64,
    requested_population_admitted: bool,
    all_accepted: bool,
    globally_ordered_fanout: bool,
    complete_live_fanout: bool,
    complete_agent_terminals: bool,
    complete_replay: bool,
    cleanup_settled: bool,
}

/// Runs independent rooms and members concurrently, preserving the complete
/// cleanup ledger until every owner DELETE has settled.
pub(crate) async fn run_saturation(
    config: SaturationConfig,
) -> Result<SaturationSummary, SaturationError> {
    config.validate()?;
    let started = Instant::now();
    let final_deadline = TokioInstant::now() + config.max_wall_time;
    let workload_deadline = final_deadline - CLEANUP_RESERVE;
    let managed_origin = exact_origin(&config.managed_url)?;
    let shape = WorkloadShape {
        rooms: config.rooms,
        guests_per_room: config.guests_per_room,
        messages_per_guest: config.messages_per_guest,
        agent_prompts_per_room: config.agent_prompts_per_room,
        connect_replay: config.connect_replay,
    };
    let record = Arc::new(Mutex::new(RunRecord::new(&config)));
    let api = RoomApi::authenticated(config.managed_url, config.account_key)?;
    let ledger = Arc::new(CleanupLedger::default());
    let mut cleanup_guard = CleanupGuard::new(api.clone(), Arc::clone(&ledger));

    let workload = run_workload(
        api.clone(),
        Arc::clone(&ledger),
        Arc::clone(&record),
        shape,
        workload_deadline,
    );
    let workload_timed_out = tokio::time::timeout_at(workload_deadline, workload)
        .await
        .is_err();
    if workload_timed_out {
        record
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .failure(FailureKey::simple("workload", "wall_time_exceeded"));
    }

    let cleanup_timed_out = cleanup_created_rooms(&api, &ledger, &record, final_deadline).await;
    cleanup_guard.settled_if_empty();
    let timed_out =
        workload_timed_out || cleanup_timed_out || TokioInstant::now() >= final_deadline;

    let pending_rooms = ledger.len() as u64;
    let mut record = Arc::try_unwrap(record)
        .map_err(|_| SaturationError::Internal("its result recorder"))?
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if pending_rooms != 0 {
        record.failure(FailureKey::simple("cleanup", "rooms_pending"));
    }
    Ok(record.finish(
        managed_origin,
        elapsed_millis(started.elapsed()),
        timed_out,
        pending_rooms,
    ))
}

fn exact_origin(url: &Url) -> Result<String, SaturationError> {
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
    {
        return Err(SaturationError::Configuration(
            "managed_url must be one exact HTTP(S) origin".to_owned(),
        ));
    }
    Ok(url.origin().ascii_serialization())
}

async fn run_workload(
    api: RoomApi,
    ledger: Arc<CleanupLedger>,
    record: Arc<Mutex<RunRecord>>,
    config: WorkloadShape,
    deadline: TokioInstant,
) {
    let rooms = create_rooms(&api, &ledger, &record, config.rooms, deadline).await;
    let rooms = join_rooms(&api, &record, rooms, config.guests_per_room, deadline).await;
    let mut clients = connect_members(&record, &rooms, deadline).await;
    if clients.is_empty() {
        record
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .violation("connect", "no_connected_members");
        return;
    }

    let tracked = exercise_live_fanout(
        &record,
        &mut clients,
        config.messages_per_guest,
        config.agent_prompts_per_room,
        deadline,
    )
    .await;
    if config.connect_replay == ConnectReplayBehavior::ReconnectAllFromZero {
        exercise_replay(&record, &mut clients, &tracked, deadline).await;
    }
}

struct RoomState {
    ordinal: usize,
    owner: Arc<CreatedRoom>,
    guests: Vec<(usize, Arc<JoinedRoom>)>,
}

async fn create_rooms(
    api: &RoomApi,
    ledger: &Arc<CleanupLedger>,
    record: &Arc<Mutex<RunRecord>>,
    count: usize,
    deadline: TokioInstant,
) -> Vec<RoomState> {
    let mut pending = FuturesUnordered::new();
    for ordinal in 0..count {
        record_lock(record).operations.create.attempt();
        let prepared = match api.prepare_create(&format!("saturation-owner-{ordinal:02}")) {
            Ok(prepared) => Arc::new(prepared),
            Err(error) => {
                record_room_failure(record, "create", &error);
                continue;
            }
        };
        let cleanup = Arc::new(CleanupEntry::new(Arc::clone(&prepared)));
        ledger.register(Arc::clone(&cleanup));
        let api = api.clone();
        let ledger = Arc::clone(ledger);
        pending.push(async move {
            let started = Instant::now();
            let result = tokio::time::timeout_at(deadline, api.execute_create(&prepared)).await;
            let result = match result {
                Ok(Ok(room)) => {
                    let owner = Arc::new(room);
                    cleanup.set_owner(Arc::clone(&owner));
                    Ok(Ok(owner))
                }
                Ok(Err(error)) => {
                    if !create_may_have_committed(&error) {
                        ledger.settle(&cleanup);
                    }
                    Ok(Err(error))
                }
                Err(elapsed) => Err(elapsed),
            };
            (ordinal, started.elapsed(), result)
        });
    }

    let mut rooms = Vec::new();
    while let Some((ordinal, elapsed, result)) = pending.next().await {
        match result {
            Ok(Ok(owner)) => {
                record_lock(record).operations.create.succeed(elapsed);
                rooms.push(RoomState {
                    ordinal,
                    owner,
                    guests: Vec::new(),
                });
            }
            Ok(Err(error)) => record_room_failure(record, "create", &error),
            Err(_) => record_lock(record).failure(FailureKey::simple("create", "timeout")),
        }
    }
    rooms.sort_by_key(|room| room.ordinal);
    rooms
}

async fn join_rooms(
    api: &RoomApi,
    record: &Arc<Mutex<RunRecord>>,
    mut rooms: Vec<RoomState>,
    guests_per_room: usize,
    deadline: TokioInstant,
) -> Vec<RoomState> {
    let mut pending = FuturesUnordered::new();
    for room in &rooms {
        for guest in 1..=guests_per_room {
            record_lock(record).operations.join.attempt();
            let api = api.clone();
            let owner = Arc::clone(&room.owner);
            let ordinal = room.ordinal;
            pending.push(async move {
                let started = Instant::now();
                let result = tokio::time::timeout_at(
                    deadline,
                    api.join(
                        owner.receipt().invitation(),
                        &format!("saturation-guest-{ordinal:02}-{guest:02}"),
                    ),
                )
                .await;
                (ordinal, guest, started.elapsed(), result)
            });
        }
    }
    let room_positions = rooms
        .iter()
        .enumerate()
        .map(|(position, room)| (room.ordinal, position))
        .collect::<HashMap<_, _>>();
    while let Some((ordinal, guest, elapsed, result)) = pending.next().await {
        match result {
            Ok(Ok(membership)) => {
                record_lock(record).operations.join.succeed(elapsed);
                if let Some(position) = room_positions.get(&ordinal) {
                    rooms[*position].guests.push((guest, Arc::new(membership)));
                } else {
                    record_lock(record).violation("join", "created_room_missing");
                }
            }
            Ok(Err(error)) => record_room_failure(record, "join", &error),
            Err(_) => record_lock(record).failure(FailureKey::simple("join", "timeout")),
        }
    }
    for room in &mut rooms {
        room.guests.sort_by_key(|(guest, _)| *guest);
    }
    rooms
}

#[derive(Clone)]
enum RetainedMembership {
    Owner(Arc<CreatedRoom>),
    Guest(Arc<JoinedRoom>),
}

impl RetainedMembership {
    async fn connect(
        &self,
        cursor: &RoomCursor,
    ) -> Result<(RoomConnection, RoomEvents), RoomError> {
        match self {
            Self::Owner(membership) => membership.connect(cursor).await,
            Self::Guest(membership) => membership.connect(cursor).await,
        }
    }
}

struct ConnectedMember {
    room: usize,
    member: usize,
    membership: RetainedMembership,
    connection: RoomConnection,
    events: Option<RoomEvents>,
}

async fn connect_members(
    record: &Arc<Mutex<RunRecord>>,
    rooms: &[RoomState],
    deadline: TokioInstant,
) -> Vec<ConnectedMember> {
    let mut pending = FuturesUnordered::new();
    for room in rooms {
        let memberships = std::iter::once((0, RetainedMembership::Owner(Arc::clone(&room.owner))))
            .chain(room.guests.iter().map(|(guest, membership)| {
                (*guest, RetainedMembership::Guest(Arc::clone(membership)))
            }));
        for (member, membership) in memberships {
            record_lock(record).operations.connect.attempt();
            let ordinal = room.ordinal;
            pending.push(async move {
                let started = Instant::now();
                let result =
                    tokio::time::timeout_at(deadline, membership.connect(&RoomCursor::zero()))
                        .await;
                (ordinal, member, membership, started.elapsed(), result)
            });
        }
    }

    let mut clients = Vec::new();
    while let Some((room, member, membership, elapsed, result)) = pending.next().await {
        match result {
            Ok(Ok((connection, events))) => {
                record_lock(record).operations.connect.succeed(elapsed);
                if member == 0
                    && (!connection.ready().can_target_agent || !connection.ready().can_end_room)
                {
                    record_lock(record).violation("connect", "owner_capabilities_changed");
                }
                clients.push(ConnectedMember {
                    room,
                    member,
                    membership,
                    connection,
                    events: Some(events),
                });
            }
            Ok(Err(error)) => record_room_failure(record, "connect", &error),
            Err(_) => record_lock(record).failure(FailureKey::simple("connect", "timeout")),
        }
    }
    clients.sort_by_key(|client| (client.room, client.member));
    clients
}

#[derive(Clone)]
struct TrackedMessage {
    id: MessageId,
    room: usize,
    sender_client: usize,
    target: RoomTarget,
    text: String,
    sent_at: Option<Instant>,
    accepted_cursor: Option<u64>,
    fanout_cursor: Option<u64>,
    seen_clients: HashSet<usize>,
    terminal_clients: HashSet<usize>,
    terminal_observed: bool,
    rejected: bool,
}

struct TrackedRun {
    events_by_room: HashMap<usize, Vec<ExpectedRoomEvent>>,
}

#[derive(Clone, PartialEq, Eq)]
struct ExpectedRoomEvent {
    cursor: u64,
    created_at: u64,
    event: RoomEventMessage,
}

enum Observed {
    Message {
        client: usize,
        message: Result<RoomServerMessage, RoomError>,
    },
    Closed {
        client: usize,
    },
}

async fn exercise_live_fanout(
    record: &Arc<Mutex<RunRecord>>,
    clients: &mut [ConnectedMember],
    messages_per_guest: usize,
    agent_prompts_per_room: usize,
    deadline: TokioInstant,
) -> TrackedRun {
    let clients_per_room = clients.iter().fold(HashMap::new(), |mut counts, client| {
        *counts.entry(client.room).or_insert(0) += 1;
        counts
    });
    let mut tracked = Vec::new();
    for (client_index, client) in clients.iter().enumerate() {
        if client.member != 0 {
            for message in 0..messages_per_guest {
                let Ok(id) = deterministic_id(format!(
                    "sat-r{:02}-g{:02}-m{:02}",
                    client.room, client.member, message
                )) else {
                    record_lock(record).violation("send", "deterministic_id_invalid");
                    continue;
                };
                tracked.push(TrackedMessage {
                    id,
                    room: client.room,
                    sender_client: client_index,
                    target: RoomTarget::Room,
                    text: format!(
                        "saturation room {:02} guest {:02} message {:02}",
                        client.room, client.member, message
                    ),
                    sent_at: None,
                    accepted_cursor: None,
                    fanout_cursor: None,
                    seen_clients: HashSet::new(),
                    terminal_clients: HashSet::new(),
                    terminal_observed: false,
                    rejected: false,
                });
            }
        } else {
            for prompt in 0..agent_prompts_per_room {
                let Ok(id) = deterministic_id(format!("sat-r{:02}-a{:02}", client.room, prompt))
                else {
                    record_lock(record).violation("send", "deterministic_id_invalid");
                    continue;
                };
                tracked.push(TrackedMessage {
                    id,
                    room: client.room,
                    sender_client: client_index,
                    target: RoomTarget::Agent,
                    text: format!(
                        "Reply with exactly: saturation-room-{:02}-prompt-{:02}",
                        client.room, prompt
                    ),
                    sent_at: None,
                    accepted_cursor: None,
                    fanout_cursor: None,
                    seen_clients: HashSet::new(),
                    terminal_clients: HashSet::new(),
                    terminal_observed: false,
                    rejected: false,
                });
            }
        }
    }
    tracked.sort_by_key(|message| {
        (
            message.room,
            clients[message.sender_client].member,
            message.id.as_str().to_owned(),
        )
    });

    {
        let mut run = record_lock(record);
        for message in &tracked {
            let room_clients = clients_per_room.get(&message.room).copied().unwrap_or(0) as u64;
            run.operations.send.attempt();
            run.operations.fanout.attempt_many(room_clients);
            run.fanout.expected_live_deliveries += room_clients;
            if message.target == RoomTarget::Agent {
                run.operations.agent_terminal.attempt();
                run.fanout.expected_agent_terminal_deliveries += room_clients;
            }
        }
    }

    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let mut readers = Vec::new();
    for (client, member) in clients.iter_mut().enumerate() {
        if let Some(events) = member.events.take() {
            readers.push(spawn_reader(client, events, observed_tx.clone()));
        } else {
            record_lock(record).violation("connect", "event_stream_missing");
        }
    }
    drop(observed_tx);

    let mut plans = BTreeMap::<usize, Vec<usize>>::new();
    for (index, message) in tracked.iter().enumerate() {
        plans.entry(message.sender_client).or_default().push(index);
    }
    let mut sends = FuturesUnordered::new();
    for (client_index, message_indices) in plans {
        let connection = &clients[client_index].connection;
        let descriptions = message_indices
            .iter()
            .map(|index| {
                (
                    *index,
                    tracked[*index].id.clone(),
                    tracked[*index].text.clone(),
                    tracked[*index].target,
                )
            })
            .collect::<Vec<_>>();
        sends.push(async move {
            let mut outcomes = Vec::new();
            for (index, id, text, target) in descriptions {
                let started = Instant::now();
                outcomes.push((index, started, connection.send(&id, &text, target).await));
            }
            outcomes
        });
    }
    while let Some(outcomes) = sends.next().await {
        for (index, started, result) in outcomes {
            match result {
                Ok(()) => tracked[index].sent_at = Some(started),
                Err(error) => {
                    tracked[index].rejected = true;
                    record_room_failure(record, "send", &error);
                }
            }
        }
    }

    let by_id = tracked
        .iter()
        .enumerate()
        .map(|(index, message)| (message.id.as_str().to_owned(), index))
        .collect::<HashMap<_, _>>();
    let initial_heads = clients
        .iter()
        .map(|client| {
            client
                .connection
                .ready()
                .latest_cursor
                .as_str()
                .parse::<u64>()
        })
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_else(|_| {
            record_lock(record).violation("fanout", "ready_cursor_out_of_range");
            vec![u64::MAX; clients.len()]
        });
    let mut client_events = vec![Vec::<ExpectedRoomEvent>::new(); clients.len()];
    let mut clients_closed = 0_usize;
    loop {
        if live_complete(&tracked, &clients_per_room, &client_events, &initial_heads)
            || clients_closed == clients.len()
        {
            break;
        }
        let observed = match tokio::time::timeout_at(deadline, observed_rx.recv()).await {
            Ok(Some(observed)) => observed,
            Ok(None) => break,
            Err(_) => {
                record_lock(record).failure(FailureKey::simple("fanout", "timeout"));
                break;
            }
        };
        match observed {
            Observed::Closed { client } => {
                clients_closed += 1;
                let _ = client;
            }
            Observed::Message { client, message } => match message {
                Ok(message) => observe_live_message(
                    record,
                    clients,
                    &by_id,
                    &mut tracked,
                    &mut client_events,
                    client,
                    message,
                ),
                Err(error) => record_room_failure(record, "fanout", &error),
            },
        }
    }

    for reader in readers {
        reader.abort();
    }
    let events_by_room = verify_live(record, clients, &tracked, &clients_per_room, &client_events);
    TrackedRun { events_by_room }
}

fn deterministic_id(value: String) -> Result<MessageId, RoomError> {
    MessageId::parse(value).map_err(RoomError::from)
}

fn spawn_reader(
    client: usize,
    mut events: RoomEvents,
    observed: mpsc::UnboundedSender<Observed>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(message) = events.next().await {
            if observed
                .send(Observed::Message { client, message })
                .is_err()
            {
                return;
            }
        }
        let _ = observed.send(Observed::Closed { client });
    })
}

fn observe_live_message(
    record: &Arc<Mutex<RunRecord>>,
    clients: &[ConnectedMember],
    by_id: &HashMap<String, usize>,
    tracked: &mut [TrackedMessage],
    client_events: &mut [Vec<ExpectedRoomEvent>],
    client: usize,
    message: RoomServerMessage,
) {
    match message {
        RoomServerMessage::Accepted {
            id,
            cursor,
            replayed,
        } => {
            let Some(&index) = by_id.get(id.as_str()) else {
                return;
            };
            let cursor = decimal_cursor(record, "accepted", &cursor);
            let tracked = &mut tracked[index];
            if tracked.sender_client != client || replayed || tracked.accepted_cursor.is_some() {
                record_lock(record).violation("accepted", "acceptance_identity_or_replay");
                return;
            }
            tracked.accepted_cursor = cursor;
            if tracked.fanout_cursor.is_some() && tracked.fanout_cursor != cursor {
                record_lock(record).violation("accepted", "accepted_cursor_changed");
            }
            if let Some(sent_at) = tracked.sent_at {
                record_lock(record)
                    .operations
                    .send
                    .succeed(sent_at.elapsed());
            }
        }
        RoomServerMessage::RoomEvent {
            cursor,
            created_at,
            event,
        } => {
            let Some(cursor) = decimal_cursor(record, "fanout", &cursor) else {
                return;
            };
            if let Some(previous) = client_events[client].last()
                && previous.cursor.checked_add(1) != Some(cursor)
            {
                record_lock(record).violation("fanout", "cursor_not_contiguous");
            }
            client_events[client].push(ExpectedRoomEvent {
                cursor,
                created_at,
                event: event.clone(),
            });
            match event {
                RoomEventMessage::MemberMessage {
                    id, text, target, ..
                } => {
                    let Some(&index) = by_id.get(id.as_str()) else {
                        return;
                    };
                    let tracked = &mut tracked[index];
                    if clients[client].room != tracked.room
                        || tracked.text != text
                        || tracked.target != target
                    {
                        record_lock(record).violation("fanout", "message_payload_changed");
                    }
                    if let Some(canonical) = tracked.fanout_cursor {
                        if canonical != cursor {
                            record_lock(record).violation("fanout", "global_cursor_changed");
                        }
                    } else {
                        tracked.fanout_cursor = Some(cursor);
                    }
                    if tracked.accepted_cursor.is_some() && tracked.accepted_cursor != Some(cursor)
                    {
                        record_lock(record).violation("fanout", "accepted_cursor_changed");
                    }
                    if tracked.seen_clients.insert(client) {
                        if let Some(sent_at) = tracked.sent_at {
                            let mut run = record_lock(record);
                            run.operations.fanout.succeed(sent_at.elapsed());
                            run.fanout.observed_live_deliveries += 1;
                        }
                    } else {
                        record_lock(record).violation("fanout", "duplicate_delivery");
                    }
                }
                RoomEventMessage::AgentMessage { reply_to, .. } => observe_agent_terminal(
                    record, clients, tracked, client, cursor, &reply_to, None,
                ),
                RoomEventMessage::AgentError { code, reply_to, .. } => observe_agent_terminal(
                    record,
                    clients,
                    tracked,
                    client,
                    cursor,
                    &reply_to,
                    Some(code),
                ),
                RoomEventMessage::MemberJoined { .. } => {}
            }
        }
        RoomServerMessage::Error { code, id, .. } => {
            record_lock(record).failure(FailureKey::websocket_code("send", &code));
            if let Some(id) = id
                && let Some(&index) = by_id.get(id.as_str())
            {
                tracked[index].rejected = true;
            }
        }
        RoomServerMessage::ReplayPaused { .. }
        | RoomServerMessage::Presence { .. }
        | RoomServerMessage::Pong { .. }
        | RoomServerMessage::Ready { .. } => {}
    }
}

fn observe_agent_terminal(
    record: &Arc<Mutex<RunRecord>>,
    clients: &[ConnectedMember],
    tracked: &mut [TrackedMessage],
    client: usize,
    cursor: u64,
    reply_to: &RoomCursor,
    error: Option<super::room::AgentErrorCode>,
) {
    let reply_to = decimal_cursor(record, "agent_terminal", reply_to);
    let Some(index) = tracked.iter().position(|message| {
        message.target == RoomTarget::Agent
            && message.accepted_cursor.is_some()
            && message.accepted_cursor == reply_to
    }) else {
        record_lock(record).violation("agent_terminal", "unknown_reply_cursor");
        return;
    };
    let message = &mut tracked[index];
    if clients[client].room != message.room {
        record_lock(record).violation("agent_terminal", "terminal_changed_room");
    }
    if message.terminal_clients.insert(client) {
        let first = !message.terminal_observed;
        message.terminal_observed = true;
        let mut run = record_lock(record);
        run.fanout.observed_agent_terminal_deliveries += 1;
        if first {
            if let Some(sent_at) = message.sent_at {
                run.operations.agent_terminal.succeed(sent_at.elapsed());
            }
            match error {
                None => run.agent_outcomes.messages += 1,
                Some(super::room::AgentErrorCode::Cancelled) => run.agent_outcomes.cancelled += 1,
                Some(super::room::AgentErrorCode::Failed) => run.agent_outcomes.failed += 1,
                Some(super::room::AgentErrorCode::Blocked) => run.agent_outcomes.blocked += 1,
                Some(super::room::AgentErrorCode::RateLimited) => {
                    run.agent_outcomes.rate_limited += 1;
                }
            }
        }
    } else {
        record_lock(record).violation("agent_terminal", "duplicate_terminal_delivery");
    }
    let _ = cursor;
}

fn decimal_cursor(
    record: &Arc<Mutex<RunRecord>>,
    phase: &'static str,
    cursor: &RoomCursor,
) -> Option<u64> {
    match cursor.as_str().parse() {
        Ok(cursor) => Some(cursor),
        Err(_) => {
            record_lock(record).violation(phase, "cursor_out_of_range");
            None
        }
    }
}

fn live_complete(
    tracked: &[TrackedMessage],
    clients_per_room: &HashMap<usize, usize>,
    client_events: &[Vec<ExpectedRoomEvent>],
    initial_heads: &[u64],
) -> bool {
    let initial_replay_complete = client_events
        .iter()
        .zip(initial_heads)
        .all(|(events, head)| {
            events
                .last()
                .map_or(*head == 0, |event| event.cursor >= *head)
        });
    initial_replay_complete
        && tracked.iter().all(|message| {
            message.rejected
                || (message.accepted_cursor.is_some()
                    && message.seen_clients.len()
                        == clients_per_room.get(&message.room).copied().unwrap_or(0)
                    && (message.target != RoomTarget::Agent
                        || message.terminal_clients.len()
                            == clients_per_room.get(&message.room).copied().unwrap_or(0)))
        })
}

fn verify_live(
    record: &Arc<Mutex<RunRecord>>,
    clients: &[ConnectedMember],
    tracked: &[TrackedMessage],
    clients_per_room: &HashMap<usize, usize>,
    client_events: &[Vec<ExpectedRoomEvent>],
) -> HashMap<usize, Vec<ExpectedRoomEvent>> {
    for message in tracked {
        if !message.rejected && message.accepted_cursor.is_none() {
            record_lock(record).violation("accepted", "acceptance_missing");
        }
        let expected_clients = clients_per_room.get(&message.room).copied().unwrap_or(0);
        if !message.rejected && message.seen_clients.len() != expected_clients {
            record_lock(record).violation("fanout", "delivery_missing");
        }
        if message.target == RoomTarget::Agent
            && !message.rejected
            && message.terminal_clients.len() != expected_clients
        {
            record_lock(record).violation("agent_terminal", "terminal_delivery_missing");
        }
    }
    for room in clients_per_room.keys() {
        let mut room_sequences = clients
            .iter()
            .enumerate()
            .filter(|(_, client)| client.room == *room)
            .map(|(client, _)| client_events[client].as_slice());
        if let Some(expected) = room_sequences.next() {
            for actual in room_sequences {
                if actual != expected {
                    record_lock(record).violation("fanout", "client_event_stream_disagreed");
                }
            }
        }
    }
    clients_per_room
        .keys()
        .filter_map(|room| {
            clients
                .iter()
                .position(|client| client.room == *room)
                .map(|client| (*room, client_events[client].clone()))
        })
        .collect()
}

async fn exercise_replay(
    record: &Arc<Mutex<RunRecord>>,
    clients: &mut [ConnectedMember],
    tracked: &TrackedRun,
    deadline: TokioInstant,
) {
    let mut pending = FuturesUnordered::new();
    for (client, member) in clients.iter().enumerate() {
        record_lock(record).operations.connect.attempt();
        record_lock(record).operations.replay.attempt();
        let membership = member.membership.clone();
        let room = member.room;
        let expected = tracked
            .events_by_room
            .get(&room)
            .cloned()
            .unwrap_or_default();
        pending.push(async move { replay_one(client, room, membership, expected, deadline).await });
    }
    while let Some(result) = pending.next().await {
        match result {
            Ok(result) => {
                let mut run = record_lock(record);
                run.operations.connect.succeed(result.connect_elapsed);
                if result.complete {
                    run.operations.replay.succeed(result.replay_elapsed);
                    run.fanout.replay_clients_completed += 1;
                } else {
                    run.violation("replay", "replay_content_incomplete");
                }
                let _ = (result.client, result.room);
            }
            Err(ReplayFailure::Room(error)) => record_room_failure(record, "replay", &error),
            Err(ReplayFailure::Timeout) => {
                record_lock(record).failure(FailureKey::simple("replay", "timeout"));
            }
            Err(ReplayFailure::Order) => {
                record_lock(record).violation("replay", "cursor_not_strictly_increasing");
            }
        }
    }
}

struct ReplayResult {
    client: usize,
    room: usize,
    connect_elapsed: Duration,
    replay_elapsed: Duration,
    complete: bool,
}

enum ReplayFailure {
    Room(RoomError),
    Timeout,
    Order,
}

async fn replay_one(
    client: usize,
    room: usize,
    membership: RetainedMembership,
    expected: Vec<ExpectedRoomEvent>,
    deadline: TokioInstant,
) -> Result<ReplayResult, ReplayFailure> {
    let started = Instant::now();
    let (connection, mut events) =
        tokio::time::timeout_at(deadline, membership.connect(&RoomCursor::zero()))
            .await
            .map_err(|_| ReplayFailure::Timeout)?
            .map_err(ReplayFailure::Room)?;
    let connect_elapsed = started.elapsed();
    let latest: u64 = connection
        .ready()
        .latest_cursor
        .as_str()
        .parse()
        .map_err(|_| ReplayFailure::Order)?;
    if expected
        .last()
        .map_or(latest != 0, |event| event.cursor != latest)
    {
        return Err(ReplayFailure::Order);
    }
    let mut previous = 0_u64;
    let mut replayed = Vec::new();
    while previous < latest {
        let message = tokio::time::timeout_at(deadline, events.next())
            .await
            .map_err(|_| ReplayFailure::Timeout)?
            .ok_or(ReplayFailure::Order)?
            .map_err(ReplayFailure::Room)?;
        if let RoomServerMessage::RoomEvent {
            cursor,
            created_at,
            event,
        } = message
        {
            let cursor: u64 = cursor.as_str().parse().map_err(|_| ReplayFailure::Order)?;
            if previous.checked_add(1) != Some(cursor) {
                return Err(ReplayFailure::Order);
            }
            previous = cursor;
            replayed.push(ExpectedRoomEvent {
                cursor,
                created_at,
                event,
            });
        }
    }
    let _ = connection.close().await;
    Ok(ReplayResult {
        client,
        room,
        connect_elapsed,
        replay_elapsed: started.elapsed(),
        complete: replayed == expected,
    })
}

struct CleanupEntry {
    create: Arc<PreparedRoomCreate>,
    owner: Mutex<Option<Arc<CreatedRoom>>>,
}

impl CleanupEntry {
    fn new(create: Arc<PreparedRoomCreate>) -> Self {
        Self {
            create,
            owner: Mutex::new(None),
        }
    }

    fn owner(&self) -> Option<Arc<CreatedRoom>> {
        self.owner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn set_owner(&self, owner: Arc<CreatedRoom>) {
        *self
            .owner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(owner);
    }
}

struct CleanupLedger<T = CleanupEntry> {
    rooms: Mutex<Vec<Arc<T>>>,
}

impl<T> Default for CleanupLedger<T> {
    fn default() -> Self {
        Self {
            rooms: Mutex::new(Vec::new()),
        }
    }
}

impl<T> CleanupLedger<T> {
    fn register(&self, room: Arc<T>) {
        self.rooms
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(room);
    }

    fn snapshot(&self) -> Vec<Arc<T>> {
        self.rooms
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn retain(&self, retain: impl FnMut(&Arc<T>) -> bool) {
        self.rooms
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(retain);
    }

    fn len(&self) -> usize {
        self.rooms
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
}

impl CleanupLedger<CleanupEntry> {
    fn settle(&self, entry: &Arc<CleanupEntry>) {
        self.retain(|pending| !Arc::ptr_eq(pending, entry));
    }
}

struct CleanupGuard {
    api: RoomApi,
    ledger: Arc<CleanupLedger>,
    settled: bool,
}

impl CleanupGuard {
    fn new(api: RoomApi, ledger: Arc<CleanupLedger>) -> Self {
        Self {
            api,
            ledger,
            settled: false,
        }
    }

    fn settled_if_empty(&mut self) {
        self.settled = self.ledger.len() == 0;
    }
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        if self.settled || self.ledger.len() == 0 {
            return;
        }
        let api = self.api.clone();
        let ledger = Arc::clone(&self.ledger);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let rooms = ledger.snapshot();
                let mut deletes = FuturesUnordered::new();
                for entry in rooms {
                    let api = api.clone();
                    deletes.push(async move {
                        let result = recover_and_delete(&api, &entry).await;
                        (entry, result)
                    });
                }
                while let Some((entry, result)) = deletes.next().await {
                    if result.is_ok() {
                        ledger.settle(&entry);
                    }
                }
            });
        }
    }
}

async fn cleanup_created_rooms(
    api: &RoomApi,
    ledger: &Arc<CleanupLedger>,
    record: &Arc<Mutex<RunRecord>>,
    deadline: TokioInstant,
) -> bool {
    let entries = ledger.snapshot();
    {
        let mut run = record_lock(record);
        run.cleanup.created_rooms = entries
            .iter()
            .filter(|entry| entry.owner().is_some())
            .count() as u64;
        run.operations.cleanup.attempt_many(entries.len() as u64);
    }
    let mut pending = FuturesUnordered::new();
    for entry in entries {
        let api = api.clone();
        pending.push(async move {
            let started = Instant::now();
            let result = tokio::time::timeout_at(deadline, recover_and_delete(&api, &entry)).await;
            (entry, started.elapsed(), result)
        });
    }
    let mut timed_out = false;
    while let Some((entry, elapsed, result)) = pending.next().await {
        match result {
            Ok(Ok(())) => {
                ledger.settle(&entry);
                let mut run = record_lock(record);
                run.operations.cleanup.succeed(elapsed);
                run.cleanup.settled_rooms += 1;
            }
            Ok(Err(error)) => record_room_failure(record, "cleanup", &error),
            Err(_) => {
                timed_out = true;
                record_lock(record).failure(FailureKey::simple("cleanup", "timeout"));
            }
        }
    }
    timed_out
}

async fn recover_and_delete(api: &RoomApi, entry: &CleanupEntry) -> Result<(), RoomError> {
    let owner = match entry.owner() {
        Some(owner) => owner,
        None => match api.execute_create(&entry.create).await {
            Ok(owner) => {
                let owner = Arc::new(owner);
                entry.set_owner(Arc::clone(&owner));
                owner
            }
            Err(error) if !create_may_have_committed(&error) => return Ok(()),
            Err(error) => return Err(error),
        },
    };
    api.delete_owned_room(&owner).await
}

fn create_may_have_committed(error: &RoomError) -> bool {
    matches!(
        error,
        RoomError::Transport(_) | RoomError::ResponseTooLarge | RoomError::InvalidReceipt(_)
    )
}

#[derive(Default)]
struct OperationSamples {
    attempted: u64,
    succeeded: u64,
    latencies_us: Vec<u64>,
}

impl OperationSamples {
    fn attempt(&mut self) {
        self.attempt_many(1);
    }

    fn attempt_many(&mut self, count: u64) {
        self.attempted += count;
    }

    fn succeed(&mut self, elapsed: Duration) {
        self.succeeded += 1;
        self.latencies_us.push(duration_micros(elapsed));
    }

    fn finish(mut self) -> OperationSummary {
        self.latencies_us.sort_unstable();
        OperationSummary {
            attempted: self.attempted,
            succeeded: self.succeeded,
            failed: self.attempted.saturating_sub(self.succeeded),
            latency_us: percentiles(&self.latencies_us),
        }
    }
}

#[derive(Default)]
struct OperationBooks {
    create: OperationSamples,
    join: OperationSamples,
    connect: OperationSamples,
    send: OperationSamples,
    fanout: OperationSamples,
    agent_terminal: OperationSamples,
    replay: OperationSamples,
    cleanup: OperationSamples,
}

impl OperationBooks {
    fn finish(self) -> OperationSummaries {
        OperationSummaries {
            create: self.create.finish(),
            join: self.join.finish(),
            connect: self.connect.finish(),
            send: self.send.finish(),
            fanout: self.fanout.finish(),
            agent_terminal: self.agent_terminal.finish(),
            replay: self.replay.finish(),
            cleanup: self.cleanup.finish(),
        }
    }
}

struct RunRecord {
    requested: RequestedLoad,
    operations: OperationBooks,
    fanout: FanoutSummary,
    agent_outcomes: AgentOutcomeSummary,
    cleanup: CleanupSummary,
    failures: BTreeMap<FailureKey, u64>,
    invariant_checks: u64,
    invariant_violations: u64,
    accepted_ok: bool,
    ordered_ok: bool,
    fanout_ok: bool,
    terminals_ok: bool,
    replay_ok: bool,
}

impl RunRecord {
    fn new(config: &SaturationConfig) -> Self {
        Self {
            requested: RequestedLoad {
                rooms: config.rooms,
                guests_per_room: config.guests_per_room,
                messages_per_guest: config.messages_per_guest,
                agent_prompts_per_room: config.agent_prompts_per_room,
                connect_replay: config.connect_replay,
                max_wall_time_ms: elapsed_millis(config.max_wall_time),
            },
            operations: OperationBooks::default(),
            fanout: FanoutSummary::default(),
            agent_outcomes: AgentOutcomeSummary::default(),
            cleanup: CleanupSummary::default(),
            failures: BTreeMap::new(),
            invariant_checks: 0,
            invariant_violations: 0,
            accepted_ok: true,
            ordered_ok: true,
            fanout_ok: true,
            terminals_ok: true,
            replay_ok: true,
        }
    }

    fn failure(&mut self, key: FailureKey) {
        *self.failures.entry(key).or_insert(0) += 1;
    }

    fn violation(&mut self, phase: &'static str, class: &'static str) {
        self.invariant_checks += 1;
        self.invariant_violations += 1;
        match phase {
            "accepted" => self.accepted_ok = false,
            "fanout" => {
                self.ordered_ok &= !class.contains("cursor") && !class.contains("order");
                self.fanout_ok = false;
            }
            "agent_terminal" => self.terminals_ok = false,
            "replay" => self.replay_ok = false,
            _ => {}
        }
        self.failure(FailureKey::simple(phase, class));
    }

    fn finish(
        mut self,
        managed_origin: String,
        elapsed_ms: u64,
        timed_out: bool,
        pending_rooms: u64,
    ) -> SaturationSummary {
        self.cleanup.pending_rooms = pending_rooms;
        self.invariant_checks += 7;
        let expected_rooms = self.requested.rooms as u64;
        let expected_joins = expected_rooms * self.requested.guests_per_room as u64;
        let expected_clients = expected_rooms * (self.requested.guests_per_room as u64 + 1);
        let expected_connects = expected_clients
            * if self.requested.connect_replay == ConnectReplayBehavior::ReconnectAllFromZero {
                2
            } else {
                1
            };
        let requested_population_admitted = self.operations.create.attempted == expected_rooms
            && self.operations.create.succeeded == expected_rooms
            && self.operations.join.attempted == expected_joins
            && self.operations.join.succeeded == expected_joins
            && self.operations.connect.attempted == expected_connects
            && self.operations.connect.succeeded == expected_connects;
        let all_accepted =
            self.accepted_ok && self.operations.send.succeeded == self.operations.send.attempted;
        let globally_ordered_fanout = self.ordered_ok;
        let complete_live_fanout = self.fanout_ok
            && self.fanout.observed_live_deliveries == self.fanout.expected_live_deliveries;
        let complete_agent_terminals = self.terminals_ok
            && self.operations.agent_terminal.succeeded == self.operations.agent_terminal.attempted
            && self.fanout.observed_agent_terminal_deliveries
                == self.fanout.expected_agent_terminal_deliveries;
        let complete_replay =
            self.replay_ok && self.operations.replay.succeeded == self.operations.replay.attempted;
        let cleanup_settled = pending_rooms == 0;
        self.invariant_violations += [
            requested_population_admitted,
            all_accepted,
            globally_ordered_fanout,
            complete_live_fanout,
            complete_agent_terminals,
            complete_replay,
            cleanup_settled,
        ]
        .into_iter()
        .filter(|passed| !passed)
        .count() as u64;
        let failures = self
            .failures
            .into_iter()
            .map(|(key, count)| FailureSummary {
                phase: key.phase,
                class: key.class,
                http_status: key.http_status,
                code: key.code,
                count,
            })
            .collect();
        SaturationSummary {
            managed_origin,
            requested: self.requested,
            elapsed_ms,
            timed_out,
            operations: self.operations.finish(),
            fanout: self.fanout,
            agent_outcomes: self.agent_outcomes,
            cleanup: self.cleanup,
            failures,
            invariants: InvariantSummary {
                checks: self.invariant_checks,
                violations: self.invariant_violations,
                requested_population_admitted,
                all_accepted,
                globally_ordered_fanout,
                complete_live_fanout,
                complete_agent_terminals,
                complete_replay,
                cleanup_settled,
            },
        }
    }
}

fn record_lock(record: &Arc<Mutex<RunRecord>>) -> std::sync::MutexGuard<'_, RunRecord> {
    record
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct FailureKey {
    phase: &'static str,
    class: &'static str,
    http_status: Option<u16>,
    code: Option<String>,
}

impl FailureKey {
    fn simple(phase: &'static str, class: &'static str) -> Self {
        Self {
            phase,
            class,
            http_status: None,
            code: None,
        }
    }

    fn websocket_code(phase: &'static str, code: &str) -> Self {
        Self {
            phase,
            class: "websocket_server",
            http_status: None,
            code: Some(safe_code(code)),
        }
    }
}

fn record_room_failure(record: &Arc<Mutex<RunRecord>>, phase: &'static str, error: &RoomError) {
    record_lock(record).failure(classify_room_error(phase, error));
}

fn classify_room_error(phase: &'static str, error: &RoomError) -> FailureKey {
    let (class, http_status, code) = match error {
        RoomError::Configuration(_) => ("configuration", None, None),
        RoomError::AuthenticationRequired => ("authentication", None, None),
        RoomError::Transport(_) => ("http_transport", None, None),
        RoomError::Http { status, code } => {
            ("http_status", Some(status.as_u16()), Some(safe_code(code)))
        }
        RoomError::ResponseTooLarge => ("http_response_too_large", None, None),
        RoomError::InvalidReceipt(_) => ("invalid_http_receipt", None, None),
        RoomError::Protocol(_) => ("websocket_protocol", None, None),
        RoomError::HandshakeTimeout => ("websocket_handshake_timeout", None, None),
        RoomError::WebSocket(error) => classify_websocket_error(error),
        RoomError::ClosedBeforeReady => ("websocket_closed_before_ready", None, None),
        RoomError::CommandChannelClosed => ("websocket_command_closed", None, None),
    };
    FailureKey {
        phase,
        class,
        http_status,
        code,
    }
}

fn classify_websocket_error(
    error: &tokio_tungstenite::tungstenite::Error,
) -> (&'static str, Option<u16>, Option<String>) {
    use tokio_tungstenite::tungstenite::Error;

    match error {
        Error::ConnectionClosed => ("websocket_closed", None, None),
        Error::AlreadyClosed => ("websocket_already_closed", None, None),
        Error::Io(error) => {
            let code = error.raw_os_error().map_or_else(
                || {
                    format!(
                        "io_{}",
                        safe_code(&format!("{:?}", error.kind()).to_lowercase())
                    )
                },
                |number| format!("os_{number}"),
            );
            ("websocket_io", None, Some(code))
        }
        Error::Tls(_) => ("websocket_tls", None, None),
        Error::Capacity(_) => ("websocket_capacity", None, None),
        Error::Protocol(_) => ("websocket_wire_protocol", None, None),
        Error::WriteBufferFull(_) => ("websocket_write_buffer_full", None, None),
        Error::Utf8(_) => ("websocket_utf8", None, None),
        Error::AttackAttempt => ("websocket_attack_attempt", None, None),
        Error::Url(_) => ("websocket_url", None, None),
        Error::Http(response) => ("websocket_http", Some(response.status().as_u16()), None),
        Error::HttpFormat(_) => ("websocket_http_format", None, None),
    }
}

fn safe_code(code: &str) -> String {
    if !code.is_empty()
        && code.len() <= 64
        && code
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        code.to_owned()
    } else {
        "invalid_error_code".to_owned()
    }
}

fn percentiles(sorted: &[u64]) -> Option<LatencyPercentiles> {
    if sorted.is_empty() {
        return None;
    }
    Some(LatencyPercentiles {
        p50: nearest_rank(sorted, 50),
        p95: nearest_rank(sorted, 95),
        p99: nearest_rank(sorted, 99),
        max: sorted[sorted.len() - 1],
    })
}

fn nearest_rank(sorted: &[u64], percentile: usize) -> u64 {
    let rank = (percentile * sorted.len()).div_ceil(100);
    sorted[rank.saturating_sub(1)]
}

fn duration_micros(duration: Duration) -> u64 {
    duration.as_micros().min(u64::MAX as u128) as u64
}

fn elapsed_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

impl fmt::Debug for SaturationConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SaturationConfig")
            .field(
                "managed_origin",
                &self.managed_url.origin().ascii_serialization(),
            )
            .field("account_key", &"[REDACTED]")
            .field("rooms", &self.rooms)
            .field("guests_per_room", &self.guests_per_room)
            .field("messages_per_guest", &self.messages_per_guest)
            .field("agent_prompts_per_room", &self.agent_prompts_per_room)
            .field("connect_replay", &self.connect_replay)
            .field("max_wall_time", &self.max_wall_time)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CleanupLedger, ConnectReplayBehavior, MAX_AGENT_PROMPTS_PER_ROOM, MAX_GUESTS_PER_ROOM,
        MAX_MESSAGES_PER_GUEST, MAX_ROOMS, MAX_WALL_TIME, MIN_WALL_TIME, OperationSamples,
        RunRecord, SaturationConfig, classify_room_error, exact_origin, nearest_rank, percentiles,
        safe_code,
    };
    use crate::room::{AccountKey, RoomError, RoomId};
    use url::Url;

    fn config() -> SaturationConfig {
        SaturationConfig {
            managed_url: Url::parse("https://managed.example/").unwrap(),
            account_key: AccountKey::parse(format!(
                "ncx_live_{}_{}",
                "a".repeat(12),
                "b".repeat(43)
            ))
            .unwrap(),
            rooms: MAX_ROOMS,
            guests_per_room: MAX_GUESTS_PER_ROOM,
            messages_per_guest: MAX_MESSAGES_PER_GUEST,
            agent_prompts_per_room: MAX_AGENT_PROMPTS_PER_ROOM,
            connect_replay: ConnectReplayBehavior::ReconnectAllFromZero,
            max_wall_time: MAX_WALL_TIME,
        }
    }

    #[test]
    fn conservative_bounds_accept_only_the_closed_safe_envelope() {
        assert!(config().validate().is_ok());

        let mut invalid = config();
        invalid.rooms = MAX_ROOMS + 1;
        assert!(invalid.validate().is_err());

        let mut invalid = config();
        invalid.guests_per_room = MAX_GUESTS_PER_ROOM + 1;
        assert!(invalid.validate().is_err());

        let mut invalid = config();
        invalid.messages_per_guest = MAX_MESSAGES_PER_GUEST + 1;
        assert!(invalid.validate().is_err());

        let mut invalid = config();
        invalid.agent_prompts_per_room = MAX_AGENT_PROMPTS_PER_ROOM + 1;
        assert!(invalid.validate().is_err());

        let mut invalid = config();
        invalid.max_wall_time = MIN_WALL_TIME - std::time::Duration::from_secs(1);
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn exact_origin_rejects_paths_credentials_queries_and_fragments() {
        assert_eq!(
            exact_origin(&Url::parse("https://managed.example/").unwrap()).unwrap(),
            "https://managed.example"
        );
        for invalid in [
            "https://managed.example/v1",
            "https://user@managed.example/",
            "https://managed.example/?other=1",
            "https://managed.example/#fragment",
        ] {
            assert!(exact_origin(&Url::parse(invalid).unwrap()).is_err());
        }
    }

    #[test]
    fn nearest_rank_percentiles_are_exact_for_small_and_large_samples() {
        assert_eq!(percentiles(&[]), None);
        let one = percentiles(&[17]).unwrap();
        assert_eq!((one.p50, one.p95, one.p99, one.max), (17, 17, 17, 17));

        let samples = (1..=100).collect::<Vec<_>>();
        let values = percentiles(&samples).unwrap();
        assert_eq!(
            (values.p50, values.p95, values.p99, values.max),
            (50, 95, 99, 100)
        );
        assert_eq!(nearest_rank(&[1, 2, 3, 4], 50), 2);
        assert_eq!(nearest_rank(&[1, 2, 3, 4], 95), 4);
    }

    #[test]
    fn operation_counts_close_with_missing_work_classified_as_failed() {
        let mut samples = OperationSamples::default();
        samples.attempt_many(3);
        samples.succeed(std::time::Duration::from_micros(9));
        samples.succeed(std::time::Duration::from_micros(3));
        let summary = samples.finish();
        assert_eq!(summary.attempted, 3);
        assert_eq!(summary.succeeded, 2);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.latency_us.unwrap().max, 9);
    }

    #[test]
    fn top_level_invariants_are_derived_from_exact_operation_counts() {
        let config = config();
        let mut record = RunRecord::new(&config);
        record.operations.send.attempt();
        record.operations.fanout.attempt();
        record.fanout.expected_live_deliveries = 1;
        let summary = record.finish("https://managed.example".to_owned(), 1, false, 0);

        assert!(!summary.invariants.requested_population_admitted);
        assert!(!summary.invariants.all_accepted);
        assert!(!summary.invariants.complete_live_fanout);
        assert!(summary.invariants.globally_ordered_fanout);
        assert!(summary.invariants.complete_agent_terminals);
        assert!(summary.invariants.complete_replay);
        assert!(summary.invariants.cleanup_settled);
        assert_eq!(summary.invariants.violations, 3);
    }

    #[test]
    fn cleanup_ledger_retains_each_room_until_settled() {
        let room = RoomId::parse(
            "0198d214-0d9d-7a45-8a89-123456789abc~AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        )
        .unwrap();
        let room = std::sync::Arc::new(room);
        let ledger = CleanupLedger::<RoomId>::default();
        ledger.register(std::sync::Arc::clone(&room));
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger.snapshot(), vec![std::sync::Arc::clone(&room)]);
        ledger.retain(|pending| pending != &room);
        assert_eq!(ledger.len(), 0);
    }

    #[test]
    fn externally_supplied_error_codes_are_bounded_before_serialization() {
        assert_eq!(safe_code("rate_limited"), "rate_limited");
        assert_eq!(safe_code("contains-secret-value!"), "invalid_error_code");
        assert_eq!(safe_code(&"a".repeat(65)), "invalid_error_code");
        assert!(!format!("{:?}", config()).contains(&"b".repeat(43)));
    }

    #[test]
    fn websocket_io_failures_retain_only_the_safe_error_kind() {
        let error = RoomError::WebSocket(tokio_tungstenite::tungstenite::Error::Io(
            std::io::Error::from(std::io::ErrorKind::ConnectionReset),
        ));
        let classified = classify_room_error("connect", &error);
        assert_eq!(classified.class, "websocket_io");
        assert_eq!(classified.code.as_deref(), Some("io_connectionreset"));
        assert_eq!(classified.http_status, None);
    }
}
