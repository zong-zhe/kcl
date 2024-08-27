use crate::analysis::{Analysis, AnalysisDatabase, DBState, OpenFileInfo};
use crate::from_lsp::file_path_from_url;
use crate::to_lsp::{kcl_diag_to_lsp_diags, url_from_path};
use crate::util::{compile, get_file_name, to_json, Params};
use crate::word_index::build_word_index;
use anyhow::Result;
use crossbeam_channel::{select, unbounded, Receiver, Sender};
use indexmap::IndexSet;
use kclvm_driver::toolchain::{self, Toolchain};
use kclvm_driver::{lookup_compile_workspaces, CompileUnitOptions, WorkSpaceKind};
use kclvm_parser::KCLModuleCache;
use kclvm_sema::core::global_state::GlobalState;
use kclvm_sema::resolver::scope::KCLScopeCache;
use lsp_server::RequestId;
use lsp_server::{ReqQueue, Request, Response};
use lsp_types::Url;
use lsp_types::{
    notification::{Notification, PublishDiagnostics},
    InitializeParams, Location, PublishDiagnosticsParams, WorkspaceFolder,
};
use parking_lot::RwLock;
use ra_ap_vfs::{ChangeKind, ChangedFile, FileId, Vfs};
use std::collections::HashMap;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, SystemTime};
use std::{sync::Arc, time::Instant};

pub(crate) type RequestHandler = fn(&mut LanguageServerState, lsp_server::Response);

/// A `Task` is something that is send from async tasks to the entry point for processing. This
/// enables synchronizing resources like the connection with the client.
#[allow(unused)]
#[derive(Debug, Clone)]
pub(crate) enum Task {
    Response(Response),
    Notify(lsp_server::Notification),
    Retry(Request),
    ChangedFile(FileId, ChangeKind),
}

#[derive(Debug, Clone)]
pub(crate) enum Event {
    Task(Task),
    Lsp(lsp_server::Message),
}

pub(crate) struct Handle<H, C> {
    pub(crate) handle: H,
    pub(crate) _receiver: C,
}

pub(crate) type KCLVfs = Arc<RwLock<Vfs>>;
pub(crate) type KCLWordIndexMap = Arc<RwLock<HashMap<Url, HashMap<String, Vec<Location>>>>>;
pub(crate) type KCLEntryCache =
    Arc<RwLock<HashMap<String, (CompileUnitOptions, Option<SystemTime>)>>>;

pub(crate) type KCLWorkSpaceConfigCache = Arc<RwLock<HashMap<WorkSpaceKind, CompileUnitOptions>>>;

pub(crate) type KCLToolChain = Arc<RwLock<dyn Toolchain>>;
pub(crate) type KCLGlobalStateCache = Arc<Mutex<GlobalState>>;

/// State for the language server
pub(crate) struct LanguageServerState {
    /// Channel to send language server messages to the client
    pub(crate) sender: Sender<lsp_server::Message>,
    /// The request queue keeps track of all incoming and outgoing requests.
    pub(crate) request_queue: lsp_server::ReqQueue<(String, Instant), RequestHandler>,
    /// Thread pool for async execution
    pub thread_pool: threadpool::ThreadPool,
    /// Channel to send tasks to from background operations
    pub task_sender: Sender<Task>,
    /// Channel to receive tasks on from background operations
    pub task_receiver: Receiver<Task>,
    /// True if the client requested that we shut down
    pub shutdown_requested: bool,
    /// The virtual filesystem that holds all the file contents
    pub vfs: KCLVfs,
    /// Holds the state of the analysis process
    pub analysis: Analysis,
    /// Documents that are currently kept in memory from the client
    pub opened_files: Arc<RwLock<HashMap<FileId, OpenFileInfo>>>,
    /// The VFS loader
    pub loader: Handle<Box<dyn ra_ap_vfs::loader::Handle>, Receiver<ra_ap_vfs::loader::Message>>,
    /// request retry time
    pub request_retry: Arc<RwLock<HashMap<RequestId, i32>>>,
    /// The word index map
    pub word_index_map: KCLWordIndexMap,
    /// KCL parse cache
    pub module_cache: KCLModuleCache,
    /// KCL resolver cache
    pub scope_cache: KCLScopeCache,
    /// KCL compile unit cache cache
    pub entry_cache: KCLEntryCache,
    /// Toolchain is used to provider KCL tool features for the language server.
    pub tool: KCLToolChain,
    /// KCL globalstate cache
    pub gs_cache: KCLGlobalStateCache,

    pub workspace_config_cache: KCLWorkSpaceConfigCache,
    /// Process files that are not in any defined workspace and delete the workspace when closing the file
    pub temporary_workspace: Arc<RwLock<HashMap<FileId, Option<WorkSpaceKind>>>>,
    pub workspace_folders: Option<Vec<WorkspaceFolder>>,
}

/// A snapshot of the state of the language server
#[allow(unused)]
pub(crate) struct LanguageServerSnapshot {
    /// The virtual filesystem that holds all the file contents
    pub vfs: Arc<RwLock<Vfs>>,
    /// Holds the state of the analysis process
    pub workspaces: Arc<RwLock<HashMap<WorkSpaceKind, DBState>>>,
    /// Documents that are currently kept in memory from the client
    pub opened_files: Arc<RwLock<HashMap<FileId, OpenFileInfo>>>,
    /// request retry time
    pub request_retry: Arc<RwLock<HashMap<RequestId, i32>>>,
    /// The word index map
    pub word_index_map: KCLWordIndexMap,
    /// KCL parse cache
    pub module_cache: KCLModuleCache,
    /// KCL resolver cache
    pub scope_cache: KCLScopeCache,
    /// KCL compile unit cache cache
    pub entry_cache: KCLEntryCache,
    /// Toolchain is used to provider KCL tool features for the language server.
    pub tool: KCLToolChain,
    /// Process files that are not in any defined workspace and delete the work
    pub temporary_workspace: Arc<RwLock<HashMap<FileId, Option<WorkSpaceKind>>>>,
}

#[allow(unused)]
impl LanguageServerState {
    pub fn new(sender: Sender<lsp_server::Message>, initialize_params: InitializeParams) -> Self {
        let (task_sender, task_receiver) = unbounded::<Task>();

        let loader = {
            let (sender, _receiver) = unbounded::<ra_ap_vfs::loader::Message>();
            let handle: ra_ap_vfs_notify::NotifyHandle =
                ra_ap_vfs::loader::Handle::spawn(Box::new(move |msg| sender.send(msg).unwrap()));
            let handle = Box::new(handle) as Box<dyn ra_ap_vfs::loader::Handle>;
            Handle { handle, _receiver }
        };

        let mut state = LanguageServerState {
            sender,
            request_queue: ReqQueue::default(),
            vfs: Arc::new(RwLock::new(Default::default())),
            thread_pool: threadpool::ThreadPool::default(),
            task_sender: task_sender.clone(),
            task_receiver,
            shutdown_requested: false,
            analysis: Analysis::default(),
            opened_files: Arc::new(RwLock::new(HashMap::new())),
            word_index_map: Arc::new(RwLock::new(HashMap::new())),
            loader,
            module_cache: KCLModuleCache::default(),
            scope_cache: KCLScopeCache::default(),
            entry_cache: KCLEntryCache::default(),
            tool: Arc::new(RwLock::new(toolchain::default())),
            gs_cache: KCLGlobalStateCache::default(),
            request_retry: Arc::new(RwLock::new(HashMap::new())),
            workspace_config_cache: KCLWorkSpaceConfigCache::default(),
            temporary_workspace: Arc::new(RwLock::new(HashMap::new())),
            workspace_folders: initialize_params.workspace_folders.clone(),
        };

        state.init_workspaces();

        let word_index_map = state.word_index_map.clone();
        state.thread_pool.execute(move || {
            if let Err(err) = update_word_index_state(word_index_map, initialize_params, true) {
                log_message(err.to_string(), &task_sender);
            }
        });

        state
    }

    /// Blocks until a new event is received from one of the many channels the language server
    /// listens to. Returns the first event that is received.
    fn next_event(&self, receiver: &Receiver<lsp_server::Message>) -> Option<Event> {
        select! {
            recv(receiver) -> msg => msg.ok().map(Event::Lsp),
            recv(self.task_receiver) -> task => Some(Event::Task(task.unwrap()))
        }
    }

    /// Runs the language server to completion
    pub fn run(mut self, receiver: Receiver<lsp_server::Message>) -> anyhow::Result<()> {
        while let Some(event) = self.next_event(&receiver) {
            if let Event::Lsp(lsp_server::Message::Notification(notification)) = &event {
                if notification.method == lsp_types::notification::Exit::METHOD {
                    return Ok(());
                }
            }
            self.handle_event(event)?;
        }
        Ok(())
    }

    /// Handles an event from one of the many sources that the language server subscribes to.
    fn handle_event(&mut self, event: Event) -> anyhow::Result<()> {
        let start_time = Instant::now();
        // 1. Process the incoming event
        match event {
            Event::Task(task) => self.handle_task(task, start_time)?,
            Event::Lsp(msg) => {
                match msg {
                    lsp_server::Message::Request(req) => self.on_request(req, start_time)?,
                    lsp_server::Message::Notification(not) => self.on_notification(not)?,
                    // lsp_server::Message::Response(resp) => self.complete_request(resp),
                    _ => {}
                }
            }
        };

        // 2. Process changes
        self.process_vfs_changes();
        Ok(())
    }

    /// Processes any and all changes that have been applied to the virtual filesystem. Generates
    /// an `AnalysisChange` and applies it if there are changes. True is returned if things changed,
    /// otherwise false.
    pub fn process_vfs_changes(&mut self) -> bool {
        // Get all the changes since the last time we processed
        let changed_files = {
            let mut vfs = self.vfs.write();
            vfs.take_changes()
        };
        if changed_files.is_empty() {
            return false;
        }

        // Construct an AnalysisChange to apply to the analysis
        for file in changed_files {
            self.process_changed_file(file);
        }
        true
    }

    /// Process vfs changed file. Update db cache when create(did_open_file), modify(did_change) or delete(did_close_file) vfs files.
    fn process_changed_file(&mut self, file: ChangedFile) {
        match file.change_kind {
            // open file
            ChangeKind::Create => {
                let filename = get_file_name(self.vfs.read(), file.file_id);
                match filename {
                    Ok(filename) => {
                        let uri = url_from_path(&filename).unwrap();
                        let mut state_workspaces = self.analysis.workspaces.read();
                        self.temporary_workspace.write().insert(file.file_id, None);

                        let mut contains = false;
                        // If some workspace has compiled this file, record open file's workspace
                        for (workspace, state) in state_workspaces.iter() {
                            match state {
                                DBState::Ready(db) => {
                                    if db.prog.get_module(&filename).is_some() {
                                        let mut openfiles = self.opened_files.write();
                                        let file_info = openfiles.get_mut(&file.file_id).unwrap();
                                        file_info.workspaces.insert(workspace.clone());
                                        drop(openfiles);
                                        contains = true;
                                    }
                                }
                                DBState::Compiling(_) | DBState::Init => {
                                    self.task_sender
                                        .send(Task::ChangedFile(file.file_id, file.change_kind))
                                        .unwrap();
                                }
                            }
                        }

                        // If all workspaces do not contain the current file, get files workspace and store in temporary_workspace
                        if !contains {
                            let tool = Arc::clone(&self.tool);
                            let (workspaces, failed) =
                                lookup_compile_workspaces(&*tool.read(), &filename, true);
                            for (workspace, opts) in workspaces {
                                self.async_compile(workspace, opts, Some(file.file_id), true);
                            }
                            if self
                                .temporary_workspace
                                .read()
                                .get(&file.file_id)
                                .unwrap_or(&None)
                                .is_none()
                            {
                                self.temporary_workspace.write().remove(&file.file_id);
                            }
                        } else {
                            self.temporary_workspace.write().remove(&file.file_id);
                        }
                    }
                    Err(err) => self.log_message(format!("{:?} not found: {}", file.file_id, err)),
                }
            }
            // edit file
            ChangeKind::Modify => {
                let filename = get_file_name(self.vfs.read(), file.file_id);
                match filename {
                    Ok(filename) => {
                        let opened_files = self.opened_files.read();
                        let file_workspaces =
                            opened_files.get(&file.file_id).unwrap().workspaces.clone();

                        // In workspace
                        if !file_workspaces.is_empty() {
                            for workspace in file_workspaces {
                                let opts = self
                                    .workspace_config_cache
                                    .read()
                                    .get(&workspace)
                                    .unwrap()
                                    .clone();

                                self.async_compile(workspace, opts, Some(file.file_id), false);
                            }
                        } else {
                            // In temporary_workspace
                            let workspace = match self.temporary_workspace.read().get(&file.file_id)
                            {
                                Some(w) => match w {
                                    Some(w) => Some(w.clone()),
                                    None => {
                                        // In compiling, retry and wait for compile complete
                                        self.task_sender
                                            .send(Task::ChangedFile(file.file_id, file.change_kind))
                                            .unwrap();
                                        None
                                    }
                                },
                                None => None,
                            };
                            if let Some(workspace) = workspace {
                                let opts = self
                                    .workspace_config_cache
                                    .read()
                                    .get(&workspace)
                                    .unwrap()
                                    .clone();

                                self.async_compile(workspace, opts, Some(file.file_id), true);
                            }
                        }
                    }
                    Err(err) => {
                        self.log_message(format!("{:?} not found: {}", file.file_id, err));
                    }
                }
            }
            // close file
            ChangeKind::Delete => {
                let mut temporary_workspace = self.temporary_workspace.write();
                if let Some(workspace) = temporary_workspace.remove(&file.file_id) {
                    let mut workspaces = self.analysis.workspaces.write();
                    if let Some(w) = workspace {
                        workspaces.remove(&w);
                    }
                }
            }
        }
    }

    /// Handles a task sent by another async task
    #[allow(clippy::unnecessary_wraps)]
    fn handle_task(&mut self, task: Task, request_received: Instant) -> anyhow::Result<()> {
        match task {
            Task::Notify(notification) => {
                self.send(notification.into());
            }
            Task::Response(response) => self.respond(response)?,
            Task::Retry(req) if !self.is_completed(&req) => {
                thread::sleep(Duration::from_millis(20));
                self.on_request(req, request_received)?
            }
            Task::Retry(_) => (),
            Task::ChangedFile(file_id, change_kind) => {
                thread::sleep(Duration::from_millis(20));
                self.process_changed_file(ChangedFile {
                    file_id,
                    change_kind,
                })
            }
        }
        Ok(())
    }

    /// Sends a response to the client. This method logs the time it took us to reply
    /// to a request from the client.
    pub(super) fn respond(&mut self, response: lsp_server::Response) -> anyhow::Result<()> {
        if let Some((method, start)) = self.request_queue.incoming.complete(response.id.clone()) {
            let duration = start.elapsed();
            self.send(response.into())?;
            self.log_message(format!(
                "Finished request {:?} in {:?} micros",
                method,
                duration.as_micros()
            ));
        }
        Ok(())
    }

    /// Sends a message to the client
    pub(crate) fn send(&self, message: lsp_server::Message) -> anyhow::Result<()> {
        self.sender.send(message)?;
        Ok(())
    }

    /// Registers a request with the server. We register all these request to make sure they all get
    /// handled and so we can measure the time it takes for them to complete from the point of view
    /// of the client.
    pub(crate) fn register_request(
        &mut self,
        request: &lsp_server::Request,
        request_received: Instant,
    ) {
        self.request_queue.incoming.register(
            request.id.clone(),
            (request.method.clone(), request_received),
        )
    }

    pub fn snapshot(&self) -> LanguageServerSnapshot {
        LanguageServerSnapshot {
            vfs: self.vfs.clone(),
            opened_files: self.opened_files.clone(),
            word_index_map: self.word_index_map.clone(),
            module_cache: self.module_cache.clone(),
            scope_cache: self.scope_cache.clone(),
            entry_cache: self.entry_cache.clone(),
            tool: self.tool.clone(),
            request_retry: self.request_retry.clone(),
            workspaces: self.analysis.workspaces.clone(),
            temporary_workspace: self.temporary_workspace.clone(),
        }
    }

    pub fn log_message(&self, message: String) {
        let typ = lsp_types::MessageType::INFO;
        let not = lsp_server::Notification::new(
            lsp_types::notification::LogMessage::METHOD.to_string(),
            lsp_types::LogMessageParams { typ, message },
        );
        self.send(not.into());
    }

    pub(crate) fn is_completed(&self, request: &lsp_server::Request) -> bool {
        self.request_queue.incoming.is_completed(&request.id)
    }

    fn init_state(&mut self) {
        self.log_message("Init state".to_string());
        self.module_cache = KCLModuleCache::default();
        self.scope_cache = KCLScopeCache::default();
        self.entry_cache = KCLEntryCache::default();
        self.gs_cache = KCLGlobalStateCache::default();
        self.workspace_config_cache = KCLWorkSpaceConfigCache::default();
        self.temporary_workspace = Arc::new(RwLock::new(HashMap::new()));
    }

    pub(crate) fn init_workspaces(&mut self) {
        self.log_message("Init workspaces".to_string());
        self.init_state();
        if let Some(workspace_folders) = &self.workspace_folders {
            for folder in workspace_folders {
                let path = file_path_from_url(&folder.uri).unwrap();
                let tool = Arc::clone(&self.tool);

                let (workspaces, failed) = lookup_compile_workspaces(&*tool.read(), &path, true);

                if let Some(failed) = failed {
                    for (key, err) in failed {
                        self.log_message(format!("parse kcl.work failed: {}: {}", key, err));
                    }
                }

                for (workspace, opts) in workspaces {
                    self.async_compile(workspace, opts, None, false);
                }
            }
        }
    }

    fn async_compile(
        &self,
        workspace: WorkSpaceKind,
        opts: CompileUnitOptions,
        changed_file_id: Option<FileId>,
        temp: bool,
    ) {
        let filename = match changed_file_id {
            Some(id) => get_file_name(self.vfs.read(), id).unwrap_or("".to_string()),
            None => "".to_string(),
        };

        let mut workspace_config_cache = self.workspace_config_cache.write();
        workspace_config_cache.insert(workspace.clone(), opts.clone());
        drop(workspace_config_cache);

        self.thread_pool.execute({
            let mut snapshot = self.snapshot();
            let sender = self.task_sender.clone();
            let module_cache = Arc::clone(&self.module_cache);
            let scope_cache = Arc::clone(&self.scope_cache);
            let entry = Arc::clone(&self.entry_cache);
            let tool = Arc::clone(&self.tool);
            let gs_cache = Arc::clone(&self.gs_cache);

            let mut files = opts.0.clone();
            move || {
                let old_diags = {
                    match snapshot.workspaces.read().get(&workspace) {
                        Some(option_db) => match option_db {
                            DBState::Ready(db) => db.diags.clone(),
                            DBState::Compiling(db) => db.diags.clone(),
                            DBState::Init => IndexSet::new(),
                        },
                        None => IndexSet::new(),
                    }
                };

                {
                    let mut workspaces = snapshot.workspaces.write();
                    let state = match workspaces.get_mut(&workspace) {
                        Some(state) => match state {
                            DBState::Ready(db) => DBState::Compiling(db.clone()),
                            DBState::Compiling(db) => DBState::Compiling(db.clone()),
                            DBState::Init => DBState::Init,
                        },
                        None => DBState::Init,
                    };
                    workspaces.insert(workspace.clone(), state);
                }
                let (diags, compile_res) = compile(
                    Params {
                        file: filename.clone(),
                        module_cache: Some(module_cache),
                        scope_cache: Some(scope_cache),
                        vfs: Some(snapshot.vfs),
                        entry_cache: Some(entry),
                        tool,
                        gs_cache: Some(gs_cache),
                    },
                    &mut files,
                    opts.1,
                );

                let mut old_diags_maps = HashMap::new();
                for diag in &old_diags {
                    let lsp_diag = kcl_diag_to_lsp_diags(diag);
                    for (key, value) in lsp_diag {
                        old_diags_maps.entry(key).or_insert(vec![]).extend(value);
                    }
                }

                // publich diags
                let mut new_diags_maps = HashMap::new();

                for diag in &diags {
                    let lsp_diag = kcl_diag_to_lsp_diags(diag);
                    for (key, value) in lsp_diag {
                        new_diags_maps.entry(key).or_insert(vec![]).extend(value);
                    }
                }

                for (file, diags) in old_diags_maps {
                    if !new_diags_maps.contains_key(&file) {
                        if let Ok(uri) = url_from_path(file) {
                            sender.send(Task::Notify(lsp_server::Notification {
                                method: PublishDiagnostics::METHOD.to_owned(),
                                params: to_json(PublishDiagnosticsParams {
                                    uri: uri.clone(),
                                    diagnostics: vec![],
                                    version: None,
                                })
                                .unwrap(),
                            }));
                        }
                    }
                }

                for (filename, diagnostics) in new_diags_maps {
                    if let Ok(uri) = url_from_path(filename) {
                        sender.send(Task::Notify(lsp_server::Notification {
                            method: PublishDiagnostics::METHOD.to_owned(),
                            params: to_json(PublishDiagnosticsParams {
                                uri: uri.clone(),
                                diagnostics,
                                version: None,
                            })
                            .unwrap(),
                        }));
                    }
                }

                match compile_res {
                    Ok((prog, gs)) => {
                        let mut workspaces = snapshot.workspaces.write();
                        workspaces.insert(
                            workspace.clone(),
                            DBState::Ready(Arc::new(AnalysisDatabase { prog, gs, diags })),
                        );
                        drop(workspaces);
                        if temp && changed_file_id.is_some() {
                            let mut temporary_workspace = snapshot.temporary_workspace.write();
                            temporary_workspace
                                .insert(changed_file_id.unwrap(), Some(workspace.clone()));
                            drop(temporary_workspace);
                        }
                    }
                    Err(_) => {
                        let mut workspaces = snapshot.workspaces.write();
                        workspaces.remove(&workspace);
                        if temp && changed_file_id.is_some() {
                            let mut temporary_workspace = snapshot.temporary_workspace.write();
                            temporary_workspace.remove(&changed_file_id.unwrap());
                            drop(temporary_workspace);
                        }
                    }
                }
            }
        })
    }
}

pub(crate) fn log_message(message: String, sender: &Sender<Task>) -> anyhow::Result<()> {
    let typ = lsp_types::MessageType::INFO;
    sender.send(Task::Notify(lsp_server::Notification::new(
        lsp_types::notification::LogMessage::METHOD.to_string(),
        lsp_types::LogMessageParams { typ, message },
    )))?;
    Ok(())
}

fn update_word_index_state(
    word_index_map: KCLWordIndexMap,
    initialize_params: InitializeParams,
    prune: bool,
) -> Result<()> {
    if let Some(workspace_folders) = initialize_params.workspace_folders {
        for folder in workspace_folders {
            let path = file_path_from_url(&folder.uri)?;
            if let Ok(word_index) = build_word_index(&path, prune) {
                word_index_map.write().insert(folder.uri, word_index);
            }
        }
    } else if let Some(root_uri) = initialize_params.root_uri {
        let path = file_path_from_url(&root_uri)?;
        if let Ok(word_index) = build_word_index(path, prune) {
            word_index_map.write().insert(root_uri, word_index);
        }
    }
    Ok(())
}
