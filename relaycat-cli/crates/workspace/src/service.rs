use crate::{FileService, GitService, ProjectRoot, ShellManager, ShellTransportEvent, WorkspaceServiceError, IMAGE_PREVIEW_LIMIT, TEXT_PREVIEW_LIMIT, IMPLEMENTED_SHELL_LIMIT};
use crate::git::next_operation_id;
use relaycat_protocol::{
    GitProgressEvent, ShellDescriptor, ShellSnapshot, WorkspaceCapabilities, WorkspaceError, WorkspaceEvent, WorkspaceEventEnvelope,
    WorkspaceRequest, WorkspaceRequestEnvelope, WorkspaceResponse, WorkspaceResponseEnvelope,
    WORKSPACE_APP_SHELL_LIMIT, WORKSPACE_DIRECTORY_ENTRY_LIMIT, WORKSPACE_DIRECTORY_PAGE_SIZE,
    WORKSPACE_GIT_HISTORY_PAGE_SIZE,
};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const IDEMPOTENCY_TTL: Duration = Duration::from_secs(10 * 60);
const IDEMPOTENCY_LIMIT: usize = 256;
/// Upper bound on queued workspace events (mostly git progress). Without it a
/// disconnected app lets the queue grow for the life of the session.
const EVENT_QUEUE_LIMIT: usize = 256;
/// Upper bound on pending cancellations. Cancelling a request id that never
/// arrives must not grow the queue forever.
const CANCEL_QUEUE_LIMIT: usize = 128;

#[derive(Clone)]
pub struct WorkspaceService {
    root: ProjectRoot,
    files: FileService,
    git: GitService,
    shells: ShellManager,
    /// Advertised in `Capabilities`; must match what `ShellManager` enforces.
    shell_limit: u8,
    cache: Arc<Mutex<HashMap<String, (Instant, std::result::Result<WorkspaceResponse, WorkspaceError>)>>>,
    cache_order: Arc<Mutex<VecDeque<String>>>,
    events: Arc<Mutex<VecDeque<WorkspaceEventEnvelope>>>,
    event_seq: Arc<Mutex<u64>>,
    /// Request ids the app asked to cancel before they started running.
    /// `execute` consumes them at entry.
    cancelled: Arc<Mutex<VecDeque<String>>>,
    /// Requests currently executing on a worker thread, mapped to the flag
    /// their git child processes poll. A `Cancel` for one of these flips the
    /// flag, which kills the running child within one poll tick.
    active: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
}

/// Removes a request from the active table when it finishes, including on
/// early return, so a cancelled (or panicking) request never leaks an entry.
struct ActiveRequest {
    active: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    request_id: String,
    flag: Arc<AtomicBool>,
}

impl Drop for ActiveRequest {
    fn drop(&mut self) {
        if let Ok(mut active) = self.active.lock() {
            // Only unregister when the table still holds *this* request's
            // flag: with a duplicated request_id the newest registration wins,
            // and the older request finishing must not evict the newer one's
            // cancel flag.
            if active
                .get(&self.request_id)
                .is_some_and(|current| Arc::ptr_eq(current, &self.flag))
            {
                active.remove(&self.request_id);
            }
        }
    }
}

impl WorkspaceService {
    pub fn new(root: ProjectRoot, shell_limit: u8) -> Self {
        // Two ceilings apply: the protocol's absolute maximum for apps, and
        // what this build actually implements (one addressable shell stream).
        let shell_limit = shell_limit
            .min(WORKSPACE_APP_SHELL_LIMIT)
            .min(IMPLEMENTED_SHELL_LIMIT);
        Self {
            files: FileService::new(root.clone()), git: GitService::new(root.clone()),
            shells: ShellManager::new(root.clone(), shell_limit), shell_limit, root,
            cache: Arc::new(Mutex::new(HashMap::new())), cache_order: Arc::new(Mutex::new(VecDeque::new())),
            events: Arc::new(Mutex::new(VecDeque::new())), event_seq: Arc::new(Mutex::new(0)),
            cancelled: Arc::new(Mutex::new(VecDeque::new())),
            active: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn project_id(&self) -> &str { self.root.project_id() }

    pub fn shell_descriptors(&self) -> Vec<ShellDescriptor> { self.shells.list() }

    pub fn ensure_workspace_shell(&self, cols: u16, rows: u16) -> Result<ShellDescriptor, WorkspaceServiceError> {
        self.ensure_workspace_shell_snapshot(cols, rows).map(|snapshot| snapshot.descriptor)
    }

    fn ensure_workspace_shell_snapshot(&self, cols: u16, rows: u16) -> Result<ShellSnapshot, WorkspaceServiceError> {
        if let Some(descriptor) = self.shells.list().into_iter().next() {
            return Ok(ShellSnapshot {
                first_output_seq: descriptor.last_output_seq,
                last_output_seq: descriptor.last_output_seq,
                descriptor,
                bytes: Vec::new(),
                complete_screen: false,
            });
        }
        self.shells.create(cols, rows)
    }

    pub fn drain_shell_transport_events(&self) -> Vec<ShellTransportEvent> {
        self.shells.drain_transport_events()
    }

    pub fn write_workspace_shell(&self, bytes: Vec<u8>) -> Result<(), WorkspaceServiceError> {
        self.shells.write_active(bytes)
    }

    pub fn resize_workspace_shell(&self, cols: u16, rows: u16) -> Result<(), WorkspaceServiceError> {
        self.shells.resize_active(cols, rows)
    }

    pub fn close_workspace_shell(&self) -> Result<(), WorkspaceServiceError> {
        let Some(shell_id) = self.shells.active_shell_id() else { return Ok(()); };
        self.shells.close(&shell_id)
    }

    pub fn execute(&self, request: WorkspaceRequestEnvelope) -> WorkspaceResponseEnvelope {
        let request_id=request.request_id.clone(); let project_id=self.root.project_id().to_string();
        if request.project_id != project_id {
            return WorkspaceResponseEnvelope{request_id,project_id,result:Err(WorkspaceServiceError::outside_project().into())};
        }
        // Cancel is resolved before dispatch: it must reach a request that is
        // already running on another worker thread, which the dispatch table
        // cannot do (it only ever sees its own operation).
        if let WorkspaceRequest::Cancel { target_request_id } = &request.operation {
            self.request_cancellation(target_request_id);
            return WorkspaceResponseEnvelope{request_id,project_id,result:Ok(WorkspaceResponse::Ack)};
        }
        // A Cancel that arrived before this request started: skip the work.
        if self.take_cancelled(&request_id) {
            return WorkspaceResponseEnvelope{request_id,project_id,result:Err(WorkspaceServiceError::cancelled().into())};
        }
        if request.deadline_unix_ms < unix_ms() {
            return WorkspaceResponseEnvelope{request_id,project_id,result:Err(WorkspaceServiceError::timeout().into())};
        }
        // The cache key includes the operation: reusing one idempotency key
        // for a different operation must not return the earlier result.
        let cache_key = request
            .idempotency_key
            .as_ref()
            .map(|key| idempotency_cache_key(&request.operation, key));
        if let Some(key) = cache_key.as_deref() {
            if let Some(result) = self.cached(key) {
                return WorkspaceResponseEnvelope{request_id,project_id,result};
            }
        }
        let (guard, cancel_flag) = self.begin_request(&request_id);
        // Registering and checking are separate steps: a Cancel that landed in
        // between is sitting in the queue, so consume it here instead of
        // running a request the app already gave up on.
        let result = if self.take_cancelled(&request_id) {
            Err(WorkspaceServiceError::cancelled().into())
        } else {
            let _scope = crate::git::enter_cancel_scope(cancel_flag);
            self.dispatch(request.operation)
        };
        drop(guard);
        // Only successful results are cached: a transient failure must not pin
        // every retry that reuses the key to the same error for ten minutes.
        if result.is_ok() {
            if let Some(key) = cache_key { self.remember(key, result.clone()); }
        }
        WorkspaceResponseEnvelope{request_id,project_id,result}
    }

    pub fn drain_events(&self) -> Vec<WorkspaceEventEnvelope> {
        self.events.lock().map(|mut events|events.drain(..).collect()).unwrap_or_default()
    }

    pub fn shutdown(&self) { let _=self.shells.close_all(); }

    fn dispatch(&self, operation:WorkspaceRequest)->std::result::Result<WorkspaceResponse,WorkspaceError>{
        let result:Result<WorkspaceResponse,WorkspaceServiceError>=match operation {
            WorkspaceRequest::Capabilities=>Ok(WorkspaceResponse::Capabilities(WorkspaceCapabilities{files:true,git_read:true,git_write:true,shell:true,directory_page_size:WORKSPACE_DIRECTORY_PAGE_SIZE,directory_entry_limit:WORKSPACE_DIRECTORY_ENTRY_LIMIT,git_history_page_size:WORKSPACE_GIT_HISTORY_PAGE_SIZE,text_preview_limit:TEXT_PREVIEW_LIMIT as u32,image_preview_limit:IMAGE_PREVIEW_LIMIT as u32,shell_limit:self.shell_limit})),
            WorkspaceRequest::Cancel{..}=>Ok(WorkspaceResponse::Ack),
            WorkspaceRequest::ListDirectory{path,offset,limit}=>self.files.list(&path,offset,limit).map(WorkspaceResponse::Directory),
            WorkspaceRequest::SearchFiles{query,limit}=>self.files.search(&query,limit).map(WorkspaceResponse::FileSearch),
            WorkspaceRequest::ReadFile{path,max_bytes,image_variant}=>self.files.read(&path,max_bytes,image_variant).map(WorkspaceResponse::File),
            WorkspaceRequest::GitSummary=>self.git.summary().map(WorkspaceResponse::GitSummary),
            WorkspaceRequest::GitStatus=>self.git.status().map(WorkspaceResponse::GitStatus),
            WorkspaceRequest::GitDiff{target,cursor,max_bytes}=>self.git.diff(target,cursor,max_bytes).map(WorkspaceResponse::GitDiff),
            WorkspaceRequest::GitHistory{query,author,reference,cursor,limit}=>self.git.history(&query,&author,&reference,cursor,limit).map(WorkspaceResponse::GitHistory),
            WorkspaceRequest::GitCommitDetail{oid,cursor,max_bytes}=>self.git.commit_detail(&oid,cursor,max_bytes).map(WorkspaceResponse::GitCommitDetail),
            WorkspaceRequest::GitListRefs=>self.git.list_refs().map(WorkspaceResponse::GitRefs),
            WorkspaceRequest::GitStage{paths}=>self.git.stage(&paths).map(WorkspaceResponse::GitMutation),
            WorkspaceRequest::GitUnstage{paths}=>self.git.unstage(&paths).map(WorkspaceResponse::GitMutation),
            WorkspaceRequest::GitDiscard{paths}=>self.git.discard(&paths).map(WorkspaceResponse::GitMutation),
            WorkspaceRequest::GitApplyPatch{patch,reverse,cached}=>self.git.apply_patch(&patch,cached,reverse).map(WorkspaceResponse::GitMutation),
            WorkspaceRequest::GitCheckout{reference}=>self.git.checkout(&reference).map(WorkspaceResponse::GitMutation),
            WorkspaceRequest::GitCreateBranch{name,start_point}=>self.git.create_branch(&name,&start_point).map(WorkspaceResponse::GitMutation),
            WorkspaceRequest::GitRenameBranch{name}=>self.git.rename_branch(&name).map(WorkspaceResponse::GitMutation),
            WorkspaceRequest::GitDeleteBranch{name,force}=>self.git.delete_branch(&name,force).map(WorkspaceResponse::GitMutation),
            WorkspaceRequest::GitCreateTag{name,target}=>self.git.create_tag(&name,&target).map(WorkspaceResponse::GitMutation),
            WorkspaceRequest::GitCommit{message,amend,push_after}=>self.git.commit(&message,amend).and_then(|commit|if push_after{self.git.remote(relaycat_protocol::GitRemoteOperation::Push)}else{Ok(commit)}).map(WorkspaceResponse::GitMutation),
            WorkspaceRequest::GitRemote{kind}=>self.git_remote(kind),
            WorkspaceRequest::GitCommitAction{kind,oid}=>self.git.commit_action(kind,&oid).map(WorkspaceResponse::GitMutation),
            WorkspaceRequest::GitReset{oid,mode}=>self.git.reset(&oid,mode).map(WorkspaceResponse::GitMutation),
            WorkspaceRequest::ShellList=>Ok(WorkspaceResponse::ShellList(self.shells.list())),
            WorkspaceRequest::ShellCreate{cols,rows}=>self.ensure_workspace_shell_snapshot(cols,rows).map(WorkspaceResponse::ShellCreated),
            WorkspaceRequest::ShellInput{..}|WorkspaceRequest::ShellResize{..}|WorkspaceRequest::ShellSnapshot{..}=>Err(WorkspaceServiceError::unsupported("shell synchronization requires terminal_stream_v2")),
            WorkspaceRequest::ShellClose{shell_id}=>self.shells.close(&shell_id).map(|_|WorkspaceResponse::Ack),
            WorkspaceRequest::ShellCloseAll=>self.shells.close_all().map(|_|WorkspaceResponse::Ack),
        }; result.map_err(Into::into)
    }

    fn git_remote(&self,kind:relaycat_protocol::GitRemoteOperation)->Result<WorkspaceResponse,WorkspaceServiceError>{
        let operation_id = next_operation_id(&format!("{kind:?}").to_ascii_lowercase());
        match self.git.remote_with_summary(kind){
            Ok(outcome)=>{
                let last=outcome.messages.len().saturating_sub(1);
                for(index,message)in outcome.messages.into_iter().enumerate(){let completed=index==last;self.push_event(WorkspaceEvent::GitProgress(GitProgressEvent{operation_id:operation_id.clone(),phase:if completed{"completed"}else{"summary"}.into(),message,percent:completed.then_some(100),completed,success:completed.then_some(true)}));}
                Ok(WorkspaceResponse::GitMutation(outcome.mutation))
            }
            Err(error)=>{
                self.push_event(WorkspaceEvent::GitProgress(GitProgressEvent{operation_id,phase:"completed".into(),message:format!("[{:?}] FAILED · {}",kind,error),percent:Some(100),completed:true,success:Some(false)}));
                Err(error)
            }
        }
    }

    fn cached(&self,key:&str)->Option<std::result::Result<WorkspaceResponse,WorkspaceError>>{let now=Instant::now();let mut cache=self.cache.lock().ok()?;cache.retain(|_,(created,_)|now.duration_since(*created)<IDEMPOTENCY_TTL);cache.get(key).map(|(_,result)|result.clone())}
    /// Marks a request as running and hands out the flag its git children poll.
    fn begin_request(&self, request_id: &str) -> (ActiveRequest, Arc<AtomicBool>) {
        let flag = Arc::new(AtomicBool::new(false));
        if let Ok(mut active) = self.active.lock() {
            // A duplicate id while one is running: the newest registration
            // wins, so a Cancel reaches the operation the app is looking at.
            active.insert(request_id.to_string(), flag.clone());
        }
        (
            ActiveRequest {
                active: self.active.clone(),
                request_id: request_id.to_string(),
                flag: flag.clone(),
            },
            flag,
        )
    }
    fn request_cancellation(&self, target_request_id: &str) {
        if let Ok(active) = self.active.lock()
            && let Some(flag) = active.get(target_request_id)
        {
            // Already running: the git poll loop sees this within one tick.
            flag.store(true, Ordering::SeqCst);
            return;
        }
        // Not started yet, or already finished: queue it so a fast retry of the
        // same id is skipped instead of doing the work.
        self.remember_cancelled(target_request_id);
    }
    fn take_cancelled(&self, request_id: &str) -> bool {
        let Ok(mut cancelled) = self.cancelled.lock() else {
            return false;
        };
        match cancelled.iter().position(|queued| queued == request_id) {
            Some(index) => {
                cancelled.remove(index);
                true
            }
            None => false,
        }
    }
    fn remember_cancelled(&self, request_id: &str) {
        let Ok(mut cancelled) = self.cancelled.lock() else {
            return;
        };
        if !cancelled.iter().any(|queued| queued == request_id) {
            cancelled.push_back(request_id.to_string());
            while cancelled.len() > CANCEL_QUEUE_LIMIT {
                cancelled.pop_front();
            }
        }
    }
    fn remember(&self,key:String,result:std::result::Result<WorkspaceResponse,WorkspaceError>){if let(Ok(mut cache),Ok(mut order))=(self.cache.lock(),self.cache_order.lock()){if !cache.contains_key(&key){order.push_back(key.clone());}cache.insert(key,(Instant::now(),result));while order.len()>IDEMPOTENCY_LIMIT{if let Some(old)=order.pop_front(){cache.remove(&old);}}}}
    fn push_event(&self,event:WorkspaceEvent){if let Ok(mut seq)=self.event_seq.lock(){*seq=seq.saturating_add(1);if let Ok(mut events)=self.events.lock(){enqueue_event(&mut events,WorkspaceEventEnvelope{project_id:self.root.project_id().into(),event_seq:*seq,event});}}}
}

/// Stable per-operation tag used in the idempotency cache key.
fn operation_tag(operation: &WorkspaceRequest) -> &'static str {
    match operation {
        WorkspaceRequest::Capabilities => "capabilities",
        WorkspaceRequest::Cancel { .. } => "cancel",
        WorkspaceRequest::ListDirectory { .. } => "list_directory",
        WorkspaceRequest::SearchFiles { .. } => "search_files",
        WorkspaceRequest::ReadFile { .. } => "read_file",
        WorkspaceRequest::GitSummary => "git_summary",
        WorkspaceRequest::GitStatus => "git_status",
        WorkspaceRequest::GitDiff { .. } => "git_diff",
        WorkspaceRequest::GitHistory { .. } => "git_history",
        WorkspaceRequest::GitCommitDetail { .. } => "git_commit_detail",
        WorkspaceRequest::GitListRefs => "git_list_refs",
        WorkspaceRequest::GitStage { .. } => "git_stage",
        WorkspaceRequest::GitUnstage { .. } => "git_unstage",
        WorkspaceRequest::GitDiscard { .. } => "git_discard",
        WorkspaceRequest::GitApplyPatch { .. } => "git_apply_patch",
        WorkspaceRequest::GitCheckout { .. } => "git_checkout",
        WorkspaceRequest::GitCreateBranch { .. } => "git_create_branch",
        WorkspaceRequest::GitRenameBranch { .. } => "git_rename_branch",
        WorkspaceRequest::GitDeleteBranch { .. } => "git_delete_branch",
        WorkspaceRequest::GitCreateTag { .. } => "git_create_tag",
        WorkspaceRequest::GitCommit { .. } => "git_commit",
        WorkspaceRequest::GitRemote { .. } => "git_remote",
        WorkspaceRequest::GitCommitAction { .. } => "git_commit_action",
        WorkspaceRequest::GitReset { .. } => "git_reset",
        WorkspaceRequest::ShellList => "shell_list",
        WorkspaceRequest::ShellCreate { .. } => "shell_create",
        WorkspaceRequest::ShellInput { .. } => "shell_input",
        WorkspaceRequest::ShellResize { .. } => "shell_resize",
        WorkspaceRequest::ShellSnapshot { .. } => "shell_snapshot",
        WorkspaceRequest::ShellClose { .. } => "shell_close",
        WorkspaceRequest::ShellCloseAll => "shell_close_all",
    }
}

fn idempotency_cache_key(operation: &WorkspaceRequest, key: &str) -> String {
    format!("{}:{key}", operation_tag(operation))
}

/// Appends an event, dropping the oldest once the queue is full. Events are
/// progress notifications (git fetch/pull/push summaries), so losing the
/// oldest under backpressure is preferable to unbounded memory growth.
fn enqueue_event(events: &mut VecDeque<WorkspaceEventEnvelope>, event: WorkspaceEventEnvelope) {
    events.push_back(event);
    while events.len() > EVENT_QUEUE_LIMIT {
        events.pop_front();
    }
}

fn unix_ms()->u64{SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis().min(u128::from(u64::MAX)) as u64}

#[cfg(test)]
mod tests {
    use super::*;
    use relaycat_protocol::GitProgressEvent;

    fn progress(seq: u64) -> WorkspaceEventEnvelope {
        WorkspaceEventEnvelope {
            project_id: "p".into(),
            event_seq: seq,
            event: WorkspaceEvent::GitProgress(GitProgressEvent {
                operation_id: format!("op-{seq}"),
                phase: "summary".into(),
                message: format!("message {seq}"),
                percent: None,
                completed: false,
                success: None,
            }),
        }
    }

    #[test]
    fn the_event_queue_drops_the_oldest_events_at_the_cap() {
        let mut events = VecDeque::new();
        let total = EVENT_QUEUE_LIMIT as u64 + 10;
        for seq in 0..total {
            enqueue_event(&mut events, progress(seq));
        }
        assert_eq!(events.len(), EVENT_QUEUE_LIMIT);
        assert_eq!(events.front().unwrap().event_seq, 10);
        assert_eq!(events.back().unwrap().event_seq, total - 1);
    }

    #[test]
    fn cancelling_a_running_request_flips_the_flag_its_git_children_poll() {
        let dir = std::env::temp_dir().join(format!(
            "relaycat-cancel-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let service = WorkspaceService::new(ProjectRoot::open(&dir).unwrap(), 1);

        let (guard, flag) = service.begin_request("slow-request");
        assert!(!flag.load(Ordering::SeqCst));

        let cancel = service.execute(WorkspaceRequestEnvelope {
            request_id: "cancel-1".into(),
            project_id: service.project_id().to_string(),
            deadline_unix_ms: u64::MAX,
            idempotency_key: None,
            operation: WorkspaceRequest::Cancel {
                target_request_id: "slow-request".into(),
            },
        });
        assert!(matches!(cancel.result.unwrap(), WorkspaceResponse::Ack));
        assert!(
            flag.load(Ordering::SeqCst),
            "a Cancel for a running request must set its flag"
        );

        // Finishing the request must not leave a stale entry behind.
        drop(guard);
        assert!(service.active.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_older_request_finishing_does_not_evict_a_newer_registration() {
        let dir = std::env::temp_dir().join(format!(
            "relaycat-cancel-dup-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let service = WorkspaceService::new(ProjectRoot::open(&dir).unwrap(), 1);

        // Two in-flight requests sharing one id: the newest registration wins,
        // so the older one finishing must leave the newer flag registered.
        let (older_guard, _older_flag) = service.begin_request("dup-request");
        let (newer_guard, newer_flag) = service.begin_request("dup-request");
        drop(older_guard);

        let cancel = service.execute(WorkspaceRequestEnvelope {
            request_id: "cancel-2".into(),
            project_id: service.project_id().to_string(),
            deadline_unix_ms: u64::MAX,
            idempotency_key: None,
            operation: WorkspaceRequest::Cancel {
                target_request_id: "dup-request".into(),
            },
        });
        assert!(matches!(cancel.result.unwrap(), WorkspaceResponse::Ack));
        assert!(
            newer_flag.load(Ordering::SeqCst),
            "the newer registration must still receive the Cancel"
        );

        drop(newer_guard);
        assert!(service.active.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cancel_for_an_unknown_request_is_still_queued() {
        let dir = std::env::temp_dir().join(format!(
            "relaycat-cancel-queue-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let service = WorkspaceService::new(ProjectRoot::open(&dir).unwrap(), 1);
        service.request_cancellation("not-started");
        assert!(service.take_cancelled("not-started"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
