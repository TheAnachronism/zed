use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use collections::{HashMap, HashSet};
use fs::Fs;
use futures::StreamExt as _;
use gpui::{
    App, AsyncApp, Context, Entity, EventEmitter, SharedString, Subscription, Task, WeakEntity,
};
use language::{
    Buffer, BufferEvent, Diagnostic, DiagnosticEntry, DiagnosticMessage, DiagnosticSourceKind,
    DiskState, PointUtf16, Unclipped,
};
use lsp::LanguageServerId;
use snippet_provider::{
    SnippetFileError, SnippetKind, SnippetSource, SourcedSnippet, parse_snippet_file,
};
use util::rel_path::RelPath;
use worktree::{Event as WorktreeEvent, PathChange, UpdatedEntriesSet, Worktree, WorktreeId};

use crate::ProjectPath;
use crate::buffer_store::BufferStore;
use crate::lsp_store::LspStore;
use crate::trusted_worktrees::{
    PathTrust, TrustedWorktrees, TrustedWorktreesEvent, TrustedWorktreesStore,
};
use crate::worktree_store::{WorktreeStore, WorktreeStoreEvent};

pub enum ProjectSnippetEvent {
    Toast {
        notification_id: SharedString,
        message: String,
    },
    HideToast {
        notification_id: SharedString,
    },
}

/// Local and SSH load `.zed/snippets` and honor trust. Collab guests are a
/// distinct mode so host files cannot be opened over the live session.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectSnippetMode {
    Load,
    CollabGuest,
}

/// Snippets discovered under one project root's `.zed/snippets` directory.
///
/// Local projects (including a collab host) load those files from disk. SSH
/// clients load them by opening the remote path as a project buffer. Collab
/// guests do not load host snippet files: sharing is through project files on
/// disk, not the live session, and guests have no trust gate. Personal and
/// extension snippets still come from [`snippet_provider::SnippetProvider`].
pub struct ProjectSnippetStore {
    worktree_store: Entity<WorktreeStore>,
    buffer_store: Entity<BufferStore>,
    lsp_store: Entity<LspStore>,
    diagnostic_server_id: LanguageServerId,
    mode: ProjectSnippetMode,
    roots: HashMap<WorktreeId, RootState>,
    loads: HashMap<(WorktreeId, Arc<RelPath>), Task<()>>,
    _worktree_store_subscription: Subscription,
    _trust_subscription: Option<Subscription>,
}

struct RootState {
    files: HashMap<Arc<RelPath>, PublishedFile>,
    buffers: HashMap<Arc<RelPath>, TrackedBuffer>,
    generations: HashMap<Arc<RelPath>, u64>,
    watching: bool,
    watch: Task<()>,
    _subscription: Subscription,
}

struct TrackedBuffer {
    buffer: Entity<Buffer>,
    _subscription: Subscription,
}

struct PublishedFile {
    kind: SnippetKind,
    snippets: Vec<Arc<snippet_provider::Snippet>>,
    has_error: bool,
    abs_path: PathBuf,
}

enum LoadedSnippetFile {
    Missing,
    Failed(String),
    Contents(String),
}

enum DirListing {
    Missing,
    Entries(Vec<String>),
}

impl EventEmitter<ProjectSnippetEvent> for ProjectSnippetStore {}

impl ProjectSnippetStore {
    pub fn new(
        worktree_store: Entity<WorktreeStore>,
        buffer_store: Entity<BufferStore>,
        lsp_store: Entity<LspStore>,
        mode: ProjectSnippetMode,
        cx: &mut Context<Self>,
    ) -> Self {
        let diagnostic_server_id = lsp_store.read(cx).languages.next_language_server_id();
        let worktree_store_subscription =
            cx.subscribe(&worktree_store, Self::on_worktree_store_event);
        let trust_subscription = if mode == ProjectSnippetMode::Load {
            TrustedWorktrees::try_get_global(cx)
                .map(|trusted| cx.subscribe(&trusted, Self::on_trusted_worktrees_event))
        } else {
            None
        };

        let mut this = Self {
            worktree_store: worktree_store.clone(),
            buffer_store,
            lsp_store,
            diagnostic_server_id,
            mode,
            roots: HashMap::default(),
            loads: HashMap::default(),
            _worktree_store_subscription: worktree_store_subscription,
            _trust_subscription: trust_subscription,
        };
        if mode == ProjectSnippetMode::Load {
            let existing = worktree_store.read(cx).worktrees().collect::<Vec<_>>();
            for worktree in existing {
                this.attach_worktree(&worktree, cx);
            }
        }
        this
    }

    /// Project snippets for a saved file in the nearest visible directory root.
    ///
    /// Language-specific snippets come first, then all-language `snippets.json`.
    /// Empty for [`ProjectSnippetMode::CollabGuest`].
    pub fn sourced_for_file(
        &self,
        language: Option<&str>,
        file: &dyn language::File,
        cx: &App,
    ) -> Vec<SourcedSnippet> {
        if self.mode == ProjectSnippetMode::CollabGuest {
            return Vec::new();
        }
        let Some(worktree_id) = self.nearest_directory_worktree(file, cx) else {
            return Vec::new();
        };
        let Some(root) = self.roots.get(&worktree_id) else {
            return Vec::new();
        };

        let mut sourced = Vec::new();
        if language.is_some() {
            extend_kind(&root.files, language, &mut sourced);
        }
        extend_kind(&root.files, None, &mut sourced);
        sourced
    }

    fn nearest_directory_worktree(
        &self,
        file: &dyn language::File,
        cx: &App,
    ) -> Option<WorktreeId> {
        let store = self.worktree_store.read(cx);
        let file_worktree = store.worktree_for_id(file.worktree_id(cx), cx)?;
        let abs_path = file_worktree.read(cx).absolutize(file.path());
        let path_style = file_worktree.read(cx).path_style();

        let mut best: Option<(usize, WorktreeId)> = None;
        for worktree in store.visible_worktrees(cx) {
            let worktree = worktree.read(cx);
            if worktree.is_single_file() {
                continue;
            }
            let root = worktree.abs_path();
            if path_style
                .strip_prefix(abs_path.as_path(), root.as_ref())
                .is_none()
            {
                continue;
            }
            let len = root.components().count();
            let replace = match best {
                Some((best_len, _)) => len > best_len,
                None => true,
            };
            if replace {
                best = Some((len, worktree.id()));
            }
        }
        best.map(|(_, id)| id)
    }

    fn on_worktree_store_event(
        &mut self,
        _: Entity<WorktreeStore>,
        event: &WorktreeStoreEvent,
        cx: &mut Context<Self>,
    ) {
        if self.mode == ProjectSnippetMode::CollabGuest {
            return;
        }
        match event {
            WorktreeStoreEvent::WorktreeAdded(worktree) => self.attach_worktree(worktree, cx),
            WorktreeStoreEvent::WorktreeRemoved(_, worktree_id)
            | WorktreeStoreEvent::WorktreeReleased(_, worktree_id) => {
                self.detach_worktree(*worktree_id, cx);
            }
            _ => {}
        }
    }

    fn on_trusted_worktrees_event(
        &mut self,
        _: Entity<TrustedWorktreesStore>,
        event: &TrustedWorktreesEvent,
        cx: &mut Context<Self>,
    ) {
        if self.mode == ProjectSnippetMode::CollabGuest {
            return;
        }
        // `can_trust` emits while this store may already be updating. Defer so
        // the handler does not re-enter the entity.
        let (trusted, store, paths) = match event {
            TrustedWorktreesEvent::Trusted(store, paths) => (true, store, paths),
            TrustedWorktreesEvent::Restricted(store, paths) => (false, store, paths),
        };
        if !self.trust_event_is_ours(store) {
            return;
        }
        let worktree_ids = self.worktree_ids_for_trust(paths, cx);
        let this = cx.weak_entity();
        cx.defer(move |cx| {
            this.update(cx, |this, cx| {
                for worktree_id in worktree_ids {
                    if trusted {
                        this.rescan_trusted(worktree_id, cx);
                    } else {
                        this.clear_published(worktree_id, cx);
                    }
                }
            })
            .ok();
        });
    }

    fn trust_event_is_ours(&self, store: &WeakEntity<WorktreeStore>) -> bool {
        store
            .upgrade()
            .is_some_and(|store| store == self.worktree_store)
    }

    fn worktree_ids_for_trust(&self, paths: &HashSet<PathTrust>, cx: &App) -> Vec<WorktreeId> {
        let mut ids = Vec::new();
        for path in paths {
            match path {
                PathTrust::Worktree(worktree_id) => ids.push(*worktree_id),
                PathTrust::AbsPath(abs_path) => {
                    for worktree in self.worktree_store.read(cx).visible_worktrees(cx) {
                        let worktree = worktree.read(cx);
                        if worktree.is_single_file() {
                            continue;
                        }
                        if worktree.abs_path().as_ref().starts_with(abs_path) {
                            ids.push(worktree.id());
                        }
                    }
                }
            }
        }
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    fn attach_worktree(&mut self, worktree: &Entity<Worktree>, cx: &mut Context<Self>) {
        if self.mode == ProjectSnippetMode::CollabGuest {
            return;
        }
        let worktree_id = worktree.read(cx).id();
        if self.roots.contains_key(&worktree_id) {
            return;
        }
        if !is_directory_root(worktree.read(cx)) {
            return;
        }

        let subscription = cx.subscribe(worktree, |this, worktree, event, cx| {
            if let WorktreeEvent::UpdatedEntries(changes) = event {
                this.on_updated_entries(&worktree, changes, cx);
            }
        });
        self.roots.insert(
            worktree_id,
            RootState {
                files: HashMap::default(),
                buffers: HashMap::default(),
                generations: HashMap::default(),
                watching: false,
                watch: Task::ready(()),
                _subscription: subscription,
            },
        );
        self.load_snapshot_entries(worktree, cx);
        self.ensure_watch(worktree_id, cx);
    }

    fn load_snapshot_entries(&mut self, worktree: &Entity<Worktree>, cx: &mut Context<Self>) {
        let worktree_id = worktree.read(cx).id();
        let snapshot = worktree.read(cx).snapshot();
        let Some(dir) = snippets_dir_rel() else {
            return;
        };
        let paths = snapshot
            .child_entries(dir)
            .filter(|entry| entry.is_file() && root_snippet_kind(&entry.path).is_some())
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>();
        for path in paths {
            self.reload_rel(worktree_id, path, cx);
        }
    }

    fn on_updated_entries(
        &mut self,
        worktree: &Entity<Worktree>,
        changes: &UpdatedEntriesSet,
        cx: &mut Context<Self>,
    ) {
        let worktree_id = worktree.read(cx).id();
        if !self.roots.contains_key(&worktree_id) {
            return;
        }
        let mut container_removed = false;
        for (path, _, change) in changes.iter() {
            if matches!(change, PathChange::Removed) && is_snippets_container(path) {
                container_removed = true;
            }
            if root_snippet_kind(path).is_none() {
                continue;
            }
            if matches!(change, PathChange::Removed) {
                self.remove_published(worktree_id, path.clone(), cx);
            } else {
                self.reload_rel(worktree_id, path.clone(), cx);
            }
        }
        if container_removed {
            self.clear_published(worktree_id, cx);
            self.mark_watch_stopped(worktree_id);
            if let Some(root) = self.roots.get_mut(&worktree_id) {
                root.watch = Task::ready(());
            }
        } else {
            self.ensure_watch(worktree_id, cx);
        }
    }

    fn ensure_watch(&mut self, worktree_id: WorktreeId, cx: &mut Context<Self>) {
        if self
            .roots
            .get(&worktree_id)
            .is_some_and(|root| root.watching)
        {
            return;
        }
        let Some(dir) = self.snippet_dir_abs(worktree_id, cx) else {
            return;
        };
        let Some(fs) = self.worktree_store.read(cx).fs() else {
            return;
        };
        if !fs.path_exists(dir.as_ref()) {
            return;
        }
        if let Some(root) = self.roots.get_mut(&worktree_id) {
            root.watching = true;
        }
        let task = cx.spawn(async move |this, cx| {
            if !Self::sync_dir(
                this.clone(),
                worktree_id,
                dir.clone(),
                fs.clone(),
                cx.clone(),
            )
            .await
            {
                this.update(cx, |this, _| this.mark_watch_stopped(worktree_id))
                    .ok();
                return;
            }
            let (mut events, _watcher) = fs.watch(dir.as_ref(), Duration::from_secs(1)).await;
            while let Some(events) = events.next().await {
                if events.is_empty() {
                    continue;
                }
                if !Self::sync_dir(
                    this.clone(),
                    worktree_id,
                    dir.clone(),
                    fs.clone(),
                    cx.clone(),
                )
                .await
                {
                    break;
                }
            }
            this.update(cx, |this, _| this.mark_watch_stopped(worktree_id))
                .ok();
        });
        if let Some(root) = self.roots.get_mut(&worktree_id) {
            root.watch = task;
        }
    }

    async fn sync_dir(
        this: WeakEntity<Self>,
        worktree_id: WorktreeId,
        dir: Arc<Path>,
        fs: Arc<dyn Fs>,
        mut cx: AsyncApp,
    ) -> bool {
        let listing = list_snippet_files(fs.as_ref(), dir.as_ref()).await;
        this.update(&mut cx, |this, cx| match listing {
            DirListing::Missing => {
                this.clear_published(worktree_id, cx);
                this.mark_watch_stopped(worktree_id);
                false
            }
            DirListing::Entries(names) => {
                this.reconcile_dir(worktree_id, &names, cx);
                true
            }
        })
        .unwrap_or(false)
    }

    fn reconcile_dir(&mut self, worktree_id: WorktreeId, names: &[String], cx: &mut Context<Self>) {
        let Some(root) = self.roots.get(&worktree_id) else {
            return;
        };
        let stale = root
            .files
            .keys()
            .filter(|path| {
                path.file_name()
                    .is_none_or(|name| !names.iter().any(|candidate| candidate == name))
            })
            .cloned()
            .collect::<Vec<_>>();
        for path in stale {
            self.remove_published(worktree_id, path, cx);
        }
        for name in names {
            let Some(path) = snippet_file_rel(name) else {
                continue;
            };
            self.reload_rel(worktree_id, path, cx);
        }
    }

    fn worktree(&self, worktree_id: WorktreeId, cx: &App) -> Option<Entity<Worktree>> {
        self.worktree_store
            .read(cx)
            .worktree_for_id(worktree_id, cx)
    }

    fn uses_local_fs(&self, worktree_id: WorktreeId, cx: &App) -> bool {
        self.worktree_store.read(cx).fs().is_some()
            && self
                .worktree(worktree_id, cx)
                .is_some_and(|worktree| worktree.read(cx).as_local().is_some())
    }

    fn reload_rel(&mut self, worktree_id: WorktreeId, path: Arc<RelPath>, cx: &mut Context<Self>) {
        let Some(kind) = root_snippet_kind(&path) else {
            return;
        };
        if !self.roots.contains_key(&worktree_id) {
            return;
        }
        if self
            .roots
            .get(&worktree_id)
            .is_some_and(|root| root.buffers.contains_key(&path))
        {
            self.republish_tracked(worktree_id, path, cx);
            return;
        }
        if self.uses_local_fs(worktree_id, cx) {
            self.reload_from_fs(worktree_id, path, kind, cx);
        } else {
            // SSH remote worktrees have no local `fs()`. Opening the path as a
            // project buffer goes through remote-server RPC. Collab guests never
            // reach here: `ProjectSnippetMode::CollabGuest` does not attach
            // worktrees, so host snippet files are not opened over the session.
            self.reload_via_buffer(worktree_id, path, kind, cx);
        }
    }

    fn reload_from_fs(
        &mut self,
        worktree_id: WorktreeId,
        path: Arc<RelPath>,
        kind: SnippetKind,
        cx: &mut Context<Self>,
    ) {
        let Some(worktree) = self.worktree(worktree_id, cx) else {
            return;
        };
        let Some(fs) = self.worktree_store.read(cx).fs() else {
            return;
        };
        let abs_path = worktree.read(cx).absolutize(&path);
        let Some(generation) = self.bump_generation(worktree_id, &path) else {
            return;
        };
        let path_for_task = path.clone();
        let task = cx.spawn(async move |this, cx| {
            let loaded = match fs.metadata(&abs_path).await {
                Ok(Some(metadata)) if metadata.is_dir => LoadedSnippetFile::Missing,
                Ok(Some(_)) => match fs.load(&abs_path).await {
                    Ok(text) => LoadedSnippetFile::Contents(text),
                    Err(error) => LoadedSnippetFile::Failed(error.to_string()),
                },
                Ok(None) => LoadedSnippetFile::Missing,
                Err(error) => LoadedSnippetFile::Failed(error.to_string()),
            };
            this.update(cx, |this, cx| {
                if !this.is_current(worktree_id, &path_for_task, generation) {
                    return;
                }
                match loaded {
                    LoadedSnippetFile::Missing => {
                        this.remove_published(worktree_id, path_for_task.clone(), cx);
                    }
                    LoadedSnippetFile::Failed(message) => {
                        this.publish_failure(
                            worktree_id,
                            path_for_task.clone(),
                            abs_path,
                            kind,
                            SnippetFileError {
                                message,
                                line: None,
                                column: None,
                            },
                            cx,
                        );
                    }
                    LoadedSnippetFile::Contents(text) => {
                        this.publish_contents(
                            worktree_id,
                            path_for_task.clone(),
                            abs_path,
                            kind,
                            &text,
                            cx,
                        );
                    }
                }
                this.loads.remove(&(worktree_id, path_for_task));
            })
            .ok();
        });
        self.loads.insert((worktree_id, path), task);
    }

    fn reload_via_buffer(
        &mut self,
        worktree_id: WorktreeId,
        path: Arc<RelPath>,
        kind: SnippetKind,
        cx: &mut Context<Self>,
    ) {
        let Some(worktree) = self.worktree(worktree_id, cx) else {
            return;
        };
        let abs_path = worktree.read(cx).absolutize(&path);
        let Some(generation) = self.bump_generation(worktree_id, &path) else {
            return;
        };
        let project_path = ProjectPath {
            worktree_id,
            path: path.clone(),
        };
        let open = self.buffer_store.update(cx, |buffer_store, cx| {
            buffer_store.open_buffer(project_path, cx)
        });
        let path_for_task = path.clone();
        let task = cx.spawn(async move |this, cx| {
            let opened = open.await;
            this.update(cx, |this, cx| {
                if !this.is_current(worktree_id, &path_for_task, generation) {
                    // A newer load owns `loads`; dropping that entry would cancel it.
                    return;
                }
                match opened {
                    Ok(buffer) => {
                        this.track_buffer(worktree_id, path_for_task.clone(), buffer, cx);
                        this.republish_tracked(worktree_id, path_for_task.clone(), cx);
                    }
                    Err(error) => {
                        this.publish_failure(
                            worktree_id,
                            path_for_task.clone(),
                            abs_path,
                            kind,
                            SnippetFileError {
                                message: error.to_string(),
                                line: None,
                                column: None,
                            },
                            cx,
                        );
                    }
                }
                this.loads.remove(&(worktree_id, path_for_task));
            })
            .ok();
        });
        self.loads.insert((worktree_id, path), task);
    }

    fn track_buffer(
        &mut self,
        worktree_id: WorktreeId,
        path: Arc<RelPath>,
        buffer: Entity<Buffer>,
        cx: &mut Context<Self>,
    ) {
        if self
            .roots
            .get(&worktree_id)
            .is_some_and(|root| root.buffers.contains_key(&path))
        {
            return;
        }
        let path_for_events = path.clone();
        let subscription = cx.subscribe(&buffer, move |this, buffer, event, cx| match event {
            BufferEvent::Reloaded | BufferEvent::Saved => {
                this.republish_tracked(worktree_id, path_for_events.clone(), cx);
            }
            BufferEvent::FileHandleChanged => {
                let present = buffer
                    .read(cx)
                    .file()
                    .is_some_and(|file| matches!(file.disk_state(), DiskState::Present { .. }));
                if present {
                    this.republish_tracked(worktree_id, path_for_events.clone(), cx);
                } else {
                    this.remove_published(worktree_id, path_for_events.clone(), cx);
                }
            }
            _ => {}
        });
        if let Some(root) = self.roots.get_mut(&worktree_id) {
            root.buffers.insert(
                path,
                TrackedBuffer {
                    buffer,
                    _subscription: subscription,
                },
            );
        }
    }

    fn republish_tracked(
        &mut self,
        worktree_id: WorktreeId,
        path: Arc<RelPath>,
        cx: &mut Context<Self>,
    ) {
        let Some(kind) = root_snippet_kind(&path) else {
            return;
        };
        let Some(buffer) = self
            .roots
            .get(&worktree_id)
            .and_then(|root| root.buffers.get(&path))
            .map(|tracked| tracked.buffer.clone())
        else {
            return;
        };
        let present = buffer
            .read(cx)
            .file()
            .is_some_and(|file| matches!(file.disk_state(), DiskState::Present { .. }));
        if !present {
            self.remove_published(worktree_id, path, cx);
            return;
        }
        let abs_path = self
            .worktree(worktree_id, cx)
            .map(|worktree| worktree.read(cx).absolutize(&path))
            .unwrap_or_else(|| PathBuf::from(path.as_unix_str()));
        let text = buffer.read(cx).text();
        self.publish_contents(worktree_id, path, abs_path, kind, &text, cx);
    }

    fn publish_contents(
        &mut self,
        worktree_id: WorktreeId,
        path: Arc<RelPath>,
        abs_path: PathBuf,
        kind: SnippetKind,
        text: &str,
        cx: &mut Context<Self>,
    ) {
        if !self.ensure_trusted(worktree_id, cx) {
            return;
        }
        match parse_snippet_file(text, abs_path.as_path()) {
            Err(error) => self.publish_failure(worktree_id, path, abs_path, kind, error, cx),
            Ok(parsed) => {
                let has_error = !parsed.errors.is_empty();
                let detail = parsed
                    .errors
                    .iter()
                    .map(|error| error.message.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                let diagnostics = diagnostics_from_errors(&parsed.errors);
                let had_error = self.insert_published(
                    worktree_id,
                    path,
                    PublishedFile {
                        kind,
                        snippets: parsed.snippets,
                        has_error,
                        abs_path: abs_path.clone(),
                    },
                );
                self.set_file_diagnostics(&abs_path, diagnostics, cx);
                if has_error {
                    self.emit_error(&abs_path, &detail, cx);
                } else if had_error == Some(true) {
                    self.emit_hide(&abs_path, cx);
                }
            }
        }
    }

    fn publish_failure(
        &mut self,
        worktree_id: WorktreeId,
        path: Arc<RelPath>,
        abs_path: PathBuf,
        kind: SnippetKind,
        error: SnippetFileError,
        cx: &mut Context<Self>,
    ) {
        if !self.ensure_trusted(worktree_id, cx) {
            return;
        }
        // A file-level JSON or read error keeps snippets already parsed from this file.
        let snippets = self
            .roots
            .get(&worktree_id)
            .and_then(|root| root.files.get(&path))
            .map(|file| file.snippets.clone())
            .unwrap_or_default();
        let diagnostics = diagnostics_from_errors(std::slice::from_ref(&error));
        self.insert_published(
            worktree_id,
            path,
            PublishedFile {
                kind,
                snippets,
                has_error: true,
                abs_path: abs_path.clone(),
            },
        );
        self.set_file_diagnostics(&abs_path, diagnostics, cx);
        self.emit_error(&abs_path, &error.message, cx);
    }

    fn insert_published(
        &mut self,
        worktree_id: WorktreeId,
        path: Arc<RelPath>,
        file: PublishedFile,
    ) -> Option<bool> {
        let root = self.roots.get_mut(&worktree_id)?;
        let had_error = root.files.get(&path).map(|existing| existing.has_error);
        root.files.insert(path, file);
        had_error
    }

    fn remove_published(
        &mut self,
        worktree_id: WorktreeId,
        path: Arc<RelPath>,
        cx: &mut Context<Self>,
    ) {
        self.bump_generation(worktree_id, &path);
        let Some(root) = self.roots.get_mut(&worktree_id) else {
            return;
        };
        root.buffers.remove(&path);
        let Some(file) = root.files.remove(&path) else {
            return;
        };
        let abs_path = file.abs_path;
        let has_error = file.has_error;
        self.clear_file_diagnostics(&abs_path, cx);
        if has_error {
            self.emit_hide(&abs_path, cx);
        }
    }

    fn clear_published(&mut self, worktree_id: WorktreeId, cx: &mut Context<Self>) {
        let Some(root) = self.roots.get_mut(&worktree_id) else {
            return;
        };
        for generation in root.generations.values_mut() {
            *generation = generation.saturating_add(1);
        }
        root.buffers.clear();
        let files = std::mem::take(&mut root.files);
        let cleared = files
            .into_values()
            .map(|file| (file.abs_path, file.has_error))
            .collect::<Vec<_>>();
        for (abs_path, has_error) in cleared {
            self.clear_file_diagnostics(&abs_path, cx);
            if has_error {
                cx.emit(ProjectSnippetEvent::HideToast {
                    notification_id: notification_id(&abs_path),
                });
            }
        }
    }

    fn mark_watch_stopped(&mut self, worktree_id: WorktreeId) {
        if let Some(root) = self.roots.get_mut(&worktree_id) {
            root.watching = false;
        }
    }

    fn rescan_trusted(&mut self, worktree_id: WorktreeId, cx: &mut Context<Self>) {
        let Some(worktree) = self.worktree(worktree_id, cx) else {
            return;
        };
        if !is_directory_root(worktree.read(cx)) {
            return;
        }
        if !self.roots.contains_key(&worktree_id) {
            self.attach_worktree(&worktree, cx);
            return;
        }
        self.load_snapshot_entries(&worktree, cx);
        self.mark_watch_stopped(worktree_id);
        if let Some(root) = self.roots.get_mut(&worktree_id) {
            root.watch = Task::ready(());
        }
        self.ensure_watch(worktree_id, cx);
    }

    fn detach_worktree(&mut self, worktree_id: WorktreeId, cx: &mut Context<Self>) {
        self.loads.retain(|(id, _), _| *id != worktree_id);
        if self.roots.contains_key(&worktree_id) {
            self.clear_published(worktree_id, cx);
            self.roots.remove(&worktree_id);
        }
    }

    fn snippet_dir_abs(&self, worktree_id: WorktreeId, cx: &App) -> Option<Arc<Path>> {
        let worktree = self.worktree(worktree_id, cx)?;
        let worktree = worktree.read(cx);
        if worktree.as_local().is_none() {
            return None;
        }
        Some(Arc::from(worktree.absolutize(snippets_dir_rel()?)))
    }

    fn bump_generation(&mut self, worktree_id: WorktreeId, path: &Arc<RelPath>) -> Option<u64> {
        let root = self.roots.get_mut(&worktree_id)?;
        let generation = root.generations.entry(path.clone()).or_insert(0);
        *generation = generation.saturating_add(1);
        Some(*generation)
    }

    fn is_current(&self, worktree_id: WorktreeId, path: &RelPath, generation: u64) -> bool {
        self.roots
            .get(&worktree_id)
            .and_then(|root| root.generations.get(path))
            .copied()
            == Some(generation)
    }

    fn ensure_trusted(&mut self, worktree_id: WorktreeId, cx: &mut Context<Self>) -> bool {
        if self.mode == ProjectSnippetMode::CollabGuest {
            return true;
        }
        let Some(trusted) = TrustedWorktrees::try_get_global(cx) else {
            return true;
        };
        trusted.update(cx, |trusted, cx| {
            trusted.can_trust(&self.worktree_store, worktree_id, cx)
        })
    }

    fn set_file_diagnostics(
        &mut self,
        abs_path: &Path,
        diagnostics: Vec<DiagnosticEntry<Unclipped<PointUtf16>>>,
        cx: &mut Context<Self>,
    ) {
        let server_id = self.diagnostic_server_id;
        let abs_path = abs_path.to_path_buf();
        let publish_summaries = self.mode == ProjectSnippetMode::Load;
        self.lsp_store.update(cx, |lsp_store, cx| {
            lsp_store.set_path_diagnostics(server_id, abs_path, diagnostics, publish_summaries, cx);
        });
    }

    fn clear_file_diagnostics(&mut self, abs_path: &Path, cx: &mut Context<Self>) {
        self.set_file_diagnostics(abs_path, Vec::new(), cx);
    }

    fn emit_error(&mut self, abs_path: &Path, detail: &str, cx: &mut Context<Self>) {
        cx.emit(ProjectSnippetEvent::Toast {
            notification_id: notification_id(abs_path),
            message: format!(
                "Invalid project snippets in {}:\n{detail}",
                abs_path.display()
            ),
        });
    }

    fn emit_hide(&mut self, abs_path: &Path, cx: &mut Context<Self>) {
        cx.emit(ProjectSnippetEvent::HideToast {
            notification_id: notification_id(abs_path),
        });
    }
}

fn diagnostics_from_errors(
    errors: &[SnippetFileError],
) -> Vec<DiagnosticEntry<Unclipped<PointUtf16>>> {
    errors
        .iter()
        .enumerate()
        .map(|(index, error)| {
            let point = Unclipped(PointUtf16::new(
                error.line.unwrap_or(0),
                error.column.unwrap_or(0),
            ));
            let mut diagnostic = Diagnostic::default();
            diagnostic.source = Some("project snippets".into());
            diagnostic.message = DiagnosticMessage::plain(error.message.clone());
            diagnostic.severity = lsp::DiagnosticSeverity::ERROR;
            diagnostic.group_id = index;
            diagnostic.is_primary = true;
            diagnostic.is_disk_based = true;
            diagnostic.source_kind = DiagnosticSourceKind::Other;
            DiagnosticEntry::new(point..point, diagnostic)
        })
        .collect()
}

fn extend_kind(
    files: &HashMap<Arc<RelPath>, PublishedFile>,
    kind: Option<&str>,
    out: &mut Vec<SourcedSnippet>,
) {
    let mut paths = files
        .iter()
        .filter(|(_, file)| file.kind.as_deref() == kind)
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        let Some(file) = files.get(&path) else {
            continue;
        };
        out.extend(file.snippets.iter().cloned().map(|snippet| SourcedSnippet {
            snippet,
            source: SnippetSource::Project,
        }));
    }
}

fn is_directory_root(worktree: &Worktree) -> bool {
    worktree.is_visible() && !worktree.is_single_file()
}

fn snippets_dir_rel() -> Option<&'static RelPath> {
    static DIR: OnceLock<Option<&'static RelPath>> = OnceLock::new();
    *DIR.get_or_init(|| RelPath::from_unix_str(".zed/snippets").ok())
}

fn snippet_file_rel(file_name: &str) -> Option<Arc<RelPath>> {
    if file_name.contains('/') || file_name.contains('\\') || !file_name.ends_with(".json") {
        return None;
    }
    let name = RelPath::from_unix_str(file_name).ok()?;
    if name.components().nth(1).is_some() {
        return None;
    }
    Some(snippets_dir_rel()?.join(name).into_arc())
}

/// `Some(kind)` when `path` is `<root>/.zed/snippets/<file>.json`.
/// `kind` is `None` for all-language `snippets.json`.
fn root_snippet_kind(path: &RelPath) -> Option<SnippetKind> {
    let mut components = path.components();
    if components.next() != Some(".zed") || components.next() != Some("snippets") {
        return None;
    }
    let file_name = components.next()?;
    if components.next().is_some() || !file_name.ends_with(".json") {
        return None;
    }
    snippet_provider::snippet_kind_from_path(path.as_std_path())
}

fn is_snippets_container(path: &RelPath) -> bool {
    path.as_unix_str() == ".zed" || path.as_unix_str() == ".zed/snippets"
}

fn notification_id(abs_path: &Path) -> SharedString {
    format!("project-snippets-{}", abs_path.display()).into()
}

async fn list_snippet_files(fs: &dyn Fs, dir: &Path) -> DirListing {
    match fs.metadata(dir).await {
        Ok(Some(metadata)) if metadata.is_dir => {}
        _ => return DirListing::Missing,
    }
    let Ok(mut entries) = fs.read_dir(dir).await else {
        return DirListing::Missing;
    };
    let mut names = Vec::new();
    while let Some(entry) = entries.next().await {
        let Ok(path) = entry else {
            continue;
        };
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.ends_with(".json") {
            names.push(name.to_owned());
        }
    }
    DirListing::Entries(names)
}
