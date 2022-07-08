use std::{
    borrow::Cow,
    cell::RefCell,
    collections::HashSet,
    fmt::Debug,
    future::Future,
    hash::Hash,
    mem::take,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex, Weak,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
use event_listener::{Event, EventListener};
use serde::{de::Visitor, Deserialize, Serialize};
use tokio::{runtime::Handle, task::JoinHandle, task_local};

use crate::{
    backend::{Backend, CellContent, CellMappings, PersistentTaskType, TransientTaskType},
    id::{BackendJobId, FunctionId, TraitTypeId},
    id_factory::IdFactory,
    raw_vc::RawVc,
    task_input::{SharedReference, SharedValue, TaskInput},
    timed_future::{self, TimedFuture},
    trace::TraceRawVcs,
    Nothing, NothingVc, TaskId, Typed, TypedForInput, ValueTypeId,
};

pub trait TurboTasksCallApi: Sync + Send {
    fn dynamic_call(&self, func: FunctionId, inputs: Vec<TaskInput>) -> RawVc;
    fn native_call(&self, func: FunctionId, inputs: Vec<TaskInput>) -> RawVc;
    fn trait_call(
        &self,
        trait_type: TraitTypeId,
        trait_fn_name: Cow<'static, str>,
        inputs: Vec<TaskInput>,
    ) -> RawVc;

    fn run_once(
        &self,
        future: Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>,
    ) -> TaskId;
}

pub trait TurboTasksApi: TurboTasksCallApi + Sync + Send {
    fn invalidate(&self, task: TaskId);

    /// Eagerly notifies all tasks that were scheduled for notifications via
    /// `schedule_notify_tasks_set()`
    fn notify_scheduled_tasks(&self);

    fn try_read_task_output(
        &self,
        task: TaskId,
        strongly_consistent: bool,
    ) -> Result<Result<RawVc, EventListener>>;
    unsafe fn try_read_task_output_untracked(
        &self,
        task: TaskId,
        strongly_consistent: bool,
    ) -> Result<Result<RawVc, EventListener>>;

    fn try_read_task_cell(
        &self,
        task: TaskId,
        index: usize,
    ) -> Result<Result<CellContent, EventListener>>;
    unsafe fn try_read_task_cell_untracked(
        &self,
        task: TaskId,
        index: usize,
    ) -> Result<Result<CellContent, EventListener>>;
    unsafe fn try_read_own_task_cell(
        &self,
        current_task: TaskId,
        index: usize,
    ) -> Result<CellContent>;

    fn get_fresh_cell(&self, task: TaskId) -> usize;
    fn read_current_task_cell(&self, index: usize) -> Result<CellContent>;
    fn update_current_task_cell(&self, index: usize, content: CellContent);
}

pub trait TaskIdProvider {
    fn get_fresh_task_id(&self) -> TaskId;
    unsafe fn reuse_task_id(&self, id: TaskId);
}

impl TaskIdProvider for IdFactory<TaskId> {
    fn get_fresh_task_id(&self) -> TaskId {
        self.get()
    }

    unsafe fn reuse_task_id(&self, id: TaskId) {
        unsafe { self.reuse(id) }
    }
}

pub trait TurboTasksBackendApi: TaskIdProvider + TurboTasksCallApi + Sync + Send {
    fn pin(&self) -> Arc<dyn TurboTasksBackendApi>;

    fn schedule(&self, task: TaskId);
    fn schedule_backend_background_job(&self, id: BackendJobId);
    fn schedule_backend_foreground_job(&self, id: BackendJobId);

    fn try_foreground_done(&self) -> Result<(), EventListener>;

    /// Enqueues tasks for notification of changed dependencies. This will
    /// eventually call `invalidate_tasks()` on all tasks.
    fn schedule_notify_tasks(&self, tasks: &Vec<TaskId>);

    /// Enqueues tasks for notification of changed dependencies. This will
    /// eventually call `invalidate_tasks()` on all tasks.
    fn schedule_notify_tasks_set(&self, tasks: &HashSet<TaskId>);
}

impl TaskIdProvider for &dyn TurboTasksBackendApi {
    fn get_fresh_task_id(&self) -> TaskId {
        (*self).get_fresh_task_id()
    }

    unsafe fn reuse_task_id(&self, id: TaskId) {
        unsafe { (*self).reuse_task_id(id) }
    }
}

impl TaskIdProvider for &dyn TaskIdProvider {
    fn get_fresh_task_id(&self) -> TaskId {
        (*self).get_fresh_task_id()
    }

    unsafe fn reuse_task_id(&self, id: TaskId) {
        unsafe { (*self).reuse_task_id(id) }
    }
}

pub struct TurboTasks<B: Backend + 'static> {
    this: Weak<Self>,
    backend: B,
    task_id_factory: IdFactory<TaskId>,
    stopped: AtomicBool,
    currently_scheduled_tasks: AtomicUsize,
    currently_scheduled_foreground_jobs: AtomicUsize,
    currently_scheduled_background_jobs: AtomicUsize,
    scheduled_tasks: AtomicUsize,
    start: Mutex<Option<Instant>>,
    last_update: Mutex<Option<(Duration, usize)>>,
    event: Event,
    event_foreground: Event,
    event_background: Event,
}

// TODO implement our own thread pool and make these thread locals instead
task_local! {
    /// The current TurboTasks instance
    static TURBO_TASKS: Arc<dyn TurboTasksApi>;

    static PREVIOUS_CELLS: RefCell<CellMappings>;

    static CURRENT_TASK_ID: TaskId;

    /// Affected [Task]s, that are tracked during task execution
    /// These tasks will be invalidated when the execution finishes
    /// or before reading a cell value
    static TASKS_TO_NOTIFY: RefCell<Vec<TaskId>>;
}

impl<B: Backend> TurboTasks<B> {
    // TODO better lifetime management for turbo tasks
    // consider using unsafe for the task_local turbo tasks
    // that should be safe as long tasks can't outlife turbo task
    // so we probably want to make sure that all tasks are joined
    // when trying to drop turbo tasks
    pub fn new(mut backend: B) -> Arc<Self> {
        let task_id_factory = IdFactory::new();
        backend.initialize(&task_id_factory);
        let this = Arc::new_cyclic(|this| Self {
            this: this.clone(),
            backend,
            task_id_factory,
            stopped: AtomicBool::new(false),
            currently_scheduled_tasks: AtomicUsize::new(0),
            currently_scheduled_background_jobs: AtomicUsize::new(0),
            currently_scheduled_foreground_jobs: AtomicUsize::new(0),
            scheduled_tasks: AtomicUsize::new(0),
            start: Default::default(),
            last_update: Default::default(),
            event: Event::new(),
            event_foreground: Event::new(),
            event_background: Event::new(),
        });
        this.backend.startup(&*this);
        this
    }

    pub fn pin(&self) -> Arc<Self> {
        self.this.upgrade().unwrap()
    }

    /// Creates a new root task
    pub fn spawn_root_task(
        &self,
        functor: impl Fn() -> Pin<Box<dyn Future<Output = Result<RawVc>> + Send>>
            + Sync
            + Send
            + 'static,
    ) -> TaskId {
        let id = self
            .backend
            .create_transient_task(TransientTaskType::Root(Box::new(functor)), self);
        self.schedule(id);
        id
    }

    // TODO make sure that all dependencies settle before reading them
    /// Creates a new root task, that is only executed once.
    /// Dependencies will not invalidate the task.
    pub fn spawn_once_task(
        &self,
        future: impl Future<Output = Result<RawVc>> + Send + 'static,
    ) -> TaskId {
        let id = self
            .backend
            .create_transient_task(TransientTaskType::Once(Box::pin(future)), self);
        self.schedule(id);
        id
    }

    pub async fn run_once<T: TraceRawVcs + Send + 'static>(
        &self,
        future: impl Future<Output = Result<T>> + Send + 'static,
    ) -> Result<T> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task_id = self.spawn_once_task(async move {
            let result = future.await?;
            tx.send(result)
                .map_err(|_| anyhow!("unable to send result"))?;
            Ok(NothingVc::new().into())
        });
        // SAFETY: A Once task will never invalidate, therefore we don't need to track a
        // dependency
        let raw_result = unsafe { read_task_output_untracked(self, task_id, false) }.await?;
        unsafe { raw_result.into_read_untracked::<Nothing>(self) }.await?;
        Ok(rx.await?)
    }

    /// Call a native function with arguments.
    /// All inputs must be resolved.
    pub(crate) fn native_call(&self, func: FunctionId, inputs: Vec<TaskInput>) -> RawVc {
        debug_assert!(inputs.iter().all(|i| i.is_resolved() && !i.is_nothing()));
        RawVc::TaskOutput(self.backend.get_or_create_persistent_task(
            PersistentTaskType::Native(func, inputs),
            current_task("turbo_function calls"),
            self,
        ))
    }

    /// Calls a native function with arguments. Resolves arguments when needed
    /// with a wrapper [Task].
    pub fn dynamic_call(&self, func: FunctionId, inputs: Vec<TaskInput>) -> RawVc {
        if inputs.iter().all(|i| i.is_resolved() && !i.is_nothing()) {
            self.native_call(func, inputs)
        } else {
            RawVc::TaskOutput(self.backend.get_or_create_persistent_task(
                PersistentTaskType::ResolveNative(func, inputs),
                current_task("turbo_function calls"),
                self,
            ))
        }
    }

    /// Calls a trait method with arguments. First input is the `self` object.
    /// Uses a wrapper task to resolve
    pub fn trait_call(
        &self,
        trait_type: TraitTypeId,
        trait_fn_name: Cow<'static, str>,
        inputs: Vec<TaskInput>,
    ) -> RawVc {
        RawVc::TaskOutput(self.backend.get_or_create_persistent_task(
            PersistentTaskType::ResolveTrait(trait_type, trait_fn_name, inputs),
            current_task("turbo_function calls"),
            self,
        ))
    }

    pub(crate) fn schedule(&self, task_id: TaskId) -> JoinHandle<()> {
        let this = self.pin();
        self.begin_foreground_task();
        self.scheduled_tasks.fetch_add(1, Ordering::AcqRel);
        #[cfg(feature = "tokio_tracing")]
        let description = this.backend.get_task_description(task_id);
        let future = TURBO_TASKS.scope(
            this.clone(),
            CURRENT_TASK_ID.scope(
                task_id,
                TASKS_TO_NOTIFY.scope(
                    Default::default(),
                    self.backend.execution_scope(task_id, async move {
                        loop {
                            if this.stopped.load(Ordering::Acquire) {
                                break;
                            }
                            if let Some(execution) =
                                this.backend.try_start_task_execution(task_id, &*this)
                            {
                                // Setup thread locals
                                let has_cell_mappings = execution.cell_mappings.is_some();

                                let cell_mappings =
                                    RefCell::new(execution.cell_mappings.unwrap_or_default());
                                let (result, duration, cell_mappings) = PREVIOUS_CELLS
                                    .scope(cell_mappings, async {
                                        let (result, duration) =
                                            TimedFuture::new(execution.future).await;
                                        let cell_mappings = if has_cell_mappings {
                                            Some(
                                                PREVIOUS_CELLS.with(|s| take(&mut *s.borrow_mut())),
                                            )
                                        } else {
                                            None
                                        };
                                        (result, duration, cell_mappings)
                                    })
                                    .await;
                                if let Err(err) = &result {
                                    println!("{} errored {}", task_id, err);
                                }
                                if duration.as_millis() > 1000 {
                                    println!(
                                        "{} took {} ms",
                                        this.backend.get_task_description(task_id),
                                        duration.as_millis()
                                    )
                                }
                                let reexecute = this.backend.task_execution_completed(
                                    task_id,
                                    cell_mappings,
                                    duration,
                                    result,
                                    &*this,
                                );
                                this.notify_scheduled_tasks_internal();
                                if !reexecute {
                                    break;
                                }
                            } else {
                                break;
                            }
                        }
                        this.finish_foreground_task();
                    }),
                ),
            ),
        );
        #[cfg(feature = "tokio_tracing")]
        return tokio::task::Builder::new().name(&description).spawn(future);
        #[cfg(not(feature = "tokio_tracing"))]
        return tokio::task::spawn(future);
    }

    fn begin_foreground_task(&self) {
        if self
            .currently_scheduled_tasks
            .fetch_add(1, Ordering::AcqRel)
            == 0
        {
            *self.start.lock().unwrap() = Some(Instant::now());
        }
    }

    fn begin_foreground_job(&self) {
        self.begin_foreground_task();
        self.currently_scheduled_foreground_jobs
            .fetch_add(1, Ordering::AcqRel);
    }

    fn finish_foreground_task(&self) {
        if self
            .currently_scheduled_tasks
            .fetch_sub(1, Ordering::AcqRel)
            == 1
        {
            // That's not super race-condition-safe, but it's only for
            // statistical reasons
            let total = self.scheduled_tasks.load(Ordering::Acquire);
            self.scheduled_tasks.store(0, Ordering::Release);
            if let Some(start) = *self.start.lock().unwrap() {
                *self.last_update.lock().unwrap() = Some((start.elapsed(), total));
            }
            self.event.notify(usize::MAX);
        }
    }

    fn finish_foreground_job(&self) {
        if self
            .currently_scheduled_foreground_jobs
            .fetch_sub(1, Ordering::AcqRel)
            == 1
        {
            self.event_foreground.notify(usize::MAX);
        }
        self.finish_foreground_task();
    }

    pub async fn wait_foreground_done(&self) {
        if self.currently_scheduled_tasks.load(Ordering::Acquire) == 0 {
            return;
        }
        let listener = self.event.listen();
        if self.currently_scheduled_tasks.load(Ordering::Acquire) != 0 {
            return;
        }
        listener.await;
    }

    pub async fn wait_done(&self) -> (Duration, usize) {
        let listener = self.event.listen();
        if self.currently_scheduled_tasks.load(Ordering::Acquire) != 0 {
            listener.await;
        }
        self.last_update.lock().unwrap().unwrap()
    }

    pub async fn wait_next_done(&self) -> (Duration, usize) {
        let listener = self.event.listen();
        listener.await;
        self.last_update.lock().unwrap().unwrap()
    }

    pub async fn wait_background_done(&self) {
        let listener = self.event_background.listen();
        if self
            .currently_scheduled_background_jobs
            .load(Ordering::Acquire)
            != 0
        {
            listener.await;
        }
    }

    pub async fn stop_and_wait(&self) {
        self.stopped.store(true, Ordering::Release);
        {
            let listener = self.event.listen();
            if self.currently_scheduled_tasks.load(Ordering::Acquire) != 0 {
                listener.await;
            }
        }
        {
            let listener = self.event_background.listen();
            if self
                .currently_scheduled_background_jobs
                .load(Ordering::Acquire)
                != 0
            {
                listener.await;
            }
        }
        self.backend.stop(self);
    }

    pub(crate) fn schedule_background_job<
        T: FnOnce(Arc<TurboTasks<B>>) -> F + Send + 'static,
        F: Future<Output = ()> + Send + 'static,
    >(
        &self,
        func: T,
    ) {
        let this = self.pin();
        self.currently_scheduled_background_jobs
            .fetch_add(1, Ordering::AcqRel);
        tokio::spawn(TURBO_TASKS.scope(this.clone(), async move {
            if this.currently_scheduled_tasks.load(Ordering::Acquire) != 0 {
                let listener = this.event.listen();
                if this.currently_scheduled_tasks.load(Ordering::Acquire) != 0 {
                    listener.await;
                }
            }
            let this2 = this.clone();
            if !this.stopped.load(Ordering::Acquire) {
                func(this).await;
            }
            if this2
                .currently_scheduled_background_jobs
                .fetch_sub(1, Ordering::AcqRel)
                == 1
            {
                this2.event_background.notify(usize::MAX);
            }
        }));
    }

    pub(crate) fn schedule_foreground_job<
        T: FnOnce(Arc<TurboTasks<B>>) -> F + Send + 'static,
        F: Future<Output = ()> + Send + 'static,
    >(
        &self,
        func: T,
    ) {
        let this = self.pin();
        this.begin_foreground_task();
        tokio::spawn(TURBO_TASKS.scope(this.clone(), async move {
            if !this.stopped.load(Ordering::Acquire) {
                func(this.clone()).await;
            }
            this.finish_foreground_task();
        }));
    }

    fn notify_scheduled_tasks_internal(&self) {
        TASKS_TO_NOTIFY.with(|tasks| {
            let tasks = tasks.take();
            if tasks.is_empty() {
                return;
            }
            self.backend.invalidate_tasks(tasks, self);
        });
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }
}

impl<B: Backend> TurboTasksCallApi for TurboTasks<B> {
    fn dynamic_call(&self, func: FunctionId, inputs: Vec<TaskInput>) -> RawVc {
        self.dynamic_call(func, inputs)
    }
    fn native_call(&self, func: FunctionId, inputs: Vec<TaskInput>) -> RawVc {
        self.native_call(func, inputs)
    }
    fn trait_call(
        &self,
        trait_type: TraitTypeId,
        trait_fn_name: Cow<'static, str>,
        inputs: Vec<TaskInput>,
    ) -> RawVc {
        self.trait_call(trait_type, trait_fn_name, inputs)
    }

    fn run_once(
        &self,
        future: Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>,
    ) -> TaskId {
        self.spawn_once_task(async move {
            future.await?;
            Ok(NothingVc::new().into())
        })
    }
}

impl<B: Backend> TurboTasksApi for TurboTasks<B> {
    fn invalidate(&self, task: TaskId) {
        self.backend.invalidate_task(task, self);
    }

    fn notify_scheduled_tasks(&self) {
        let _ = TASKS_TO_NOTIFY.try_with(|tasks| {
            let tasks = tasks.take();
            if tasks.is_empty() {
                return;
            }
            self.backend.invalidate_tasks(tasks, self);
        });
    }

    fn try_read_task_output(
        &self,
        task: TaskId,
        strongly_consistent: bool,
    ) -> Result<Result<RawVc, EventListener>> {
        self.backend.try_read_task_output(
            task,
            current_task("reading Vcs"),
            strongly_consistent,
            self,
        )
    }

    unsafe fn try_read_task_output_untracked(
        &self,
        task: TaskId,
        strongly_consistent: bool,
    ) -> Result<Result<RawVc, EventListener>> {
        unsafe {
            self.backend
                .try_read_task_output_untracked(task, strongly_consistent, self)
        }
    }

    fn try_read_task_cell(
        &self,
        task: TaskId,
        index: usize,
    ) -> Result<Result<CellContent, EventListener>> {
        self.backend
            .try_read_task_cell(task, index, current_task("reading Vcs"), self)
    }

    unsafe fn try_read_task_cell_untracked(
        &self,
        task: TaskId,
        index: usize,
    ) -> Result<Result<CellContent, EventListener>> {
        unsafe { self.backend.try_read_task_cell_untracked(task, index, self) }
    }

    unsafe fn try_read_own_task_cell(
        &self,
        current_task: TaskId,
        index: usize,
    ) -> Result<CellContent> {
        unsafe {
            self.backend
                .try_read_own_task_cell(current_task, index, self)
        }
    }

    fn get_fresh_cell(&self, task: TaskId) -> usize {
        self.backend.get_fresh_cell(task, self)
    }

    fn read_current_task_cell(&self, index: usize) -> Result<CellContent> {
        unsafe { Ok(self.try_read_own_task_cell(current_task("reading Vcs"), index)?) }
    }

    fn update_current_task_cell(&self, index: usize, content: CellContent) {
        self.backend.update_task_cell(
            current_task("cellting turbo_tasks values"),
            index,
            content,
            self,
        );
    }
}

impl<B: Backend> TurboTasksBackendApi for TurboTasks<B> {
    fn pin(&self) -> Arc<dyn TurboTasksBackendApi> {
        self.pin()
    }
    fn schedule_backend_background_job(&self, id: BackendJobId) {
        self.schedule_background_job(move |this| async move {
            this.backend.run_backend_job(id, &*this).await;
        })
    }
    fn schedule_backend_foreground_job(&self, id: BackendJobId) {
        self.schedule_foreground_job(move |this| async move {
            this.backend.run_backend_job(id, &*this).await;
        })
    }

    fn try_foreground_done(&self) -> Result<(), EventListener> {
        if self.currently_scheduled_tasks.load(Ordering::Acquire) == 0 {
            return Ok(());
        }
        let listener = self.event.listen();
        if self.currently_scheduled_tasks.load(Ordering::Acquire) != 0 {
            return Ok(());
        }
        Err(listener)
    }

    /// Enqueues tasks for notification of changed dependencies. This will
    /// eventually call `dependent_cell_updated()` on all tasks.
    fn schedule_notify_tasks(&self, tasks: &Vec<TaskId>) {
        TASKS_TO_NOTIFY.with(|tasks_list| {
            let mut list = tasks_list.borrow_mut();
            list.extend(tasks.iter());
        });
    }

    /// Enqueues tasks for notification of changed dependencies. This will
    /// eventually call `dependent_cell_updated()` on all tasks.
    fn schedule_notify_tasks_set(&self, tasks: &HashSet<TaskId>) {
        TASKS_TO_NOTIFY.with(|tasks_list| {
            let mut list = tasks_list.borrow_mut();
            list.extend(tasks.iter());
        });
    }

    fn schedule(&self, task: TaskId) {
        self.schedule(task);
    }
}

impl<B: Backend> TaskIdProvider for TurboTasks<B> {
    fn get_fresh_task_id(&self) -> TaskId {
        self.task_id_factory.get()
    }

    unsafe fn reuse_task_id(&self, id: TaskId) {
        unsafe { self.task_id_factory.reuse(id) }
    }
}

fn current_task(from: &str) -> TaskId {
    match CURRENT_TASK_ID.try_with(|id| *id) {
        Ok(id) => id,
        Err(_) => panic!(
            "{} can only be used in the context of turbo_tasks task execution",
            from
        ),
    }
}

pub struct Invalidator {
    task: TaskId,
    turbo_tasks: Weak<dyn TurboTasksApi>,
    handle: Handle,
}

impl Invalidator {
    pub fn invalidate(self) {
        let Invalidator {
            task,
            turbo_tasks,
            handle,
        } = self;
        handle.spawn(async move {
            if let Some(turbo_tasks) = turbo_tasks.upgrade() {
                turbo_tasks.invalidate(task);
            }
        });
    }
}

impl TraceRawVcs for Invalidator {
    fn trace_raw_vcs(&self, _context: &mut crate::trace::TraceRawVcsContext) {
        // nothing here
    }
}

impl Serialize for Invalidator {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_newtype_struct("Invalidator", &self.task)
    }
}

impl<'de> Deserialize<'de> for Invalidator {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct V;

        impl<'de> Visitor<'de> for V {
            type Value = Invalidator;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "an Invalidator")
            }

            fn visit_newtype_struct<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                Ok(Invalidator {
                    task: TaskId::deserialize(deserializer)?,
                    turbo_tasks: weak_turbo_tasks(),
                    handle: tokio::runtime::Handle::current(),
                })
            }
        }
        deserializer.deserialize_newtype_struct("Invalidator", V)
    }
}

/// see [TurboTasks] `dynamic_call`
pub fn dynamic_call(func: FunctionId, inputs: Vec<TaskInput>) -> RawVc {
    with_turbo_tasks(|tt| tt.dynamic_call(func, inputs))
}

/// see [TurboTasks] `trait_call`
pub fn trait_call(
    trait_type: TraitTypeId,
    trait_fn_name: Cow<'static, str>,
    inputs: Vec<TaskInput>,
) -> RawVc {
    with_turbo_tasks(|tt| tt.trait_call(trait_type, trait_fn_name, inputs))
}

pub fn turbo_tasks() -> Arc<dyn TurboTasksApi> {
    TURBO_TASKS.with(|arc| arc.clone())
}

pub fn with_turbo_tasks<T>(func: impl FnOnce(&Arc<dyn TurboTasksApi>) -> T) -> T {
    TURBO_TASKS.with(|arc| func(arc))
}

pub fn weak_turbo_tasks() -> Weak<dyn TurboTasksApi> {
    TURBO_TASKS.with(|arc| Arc::downgrade(arc))
}

pub unsafe fn with_turbo_tasks_for_testing<T>(
    tt: Arc<dyn TurboTasksApi>,
    current_task: TaskId,
    f: impl Future<Output = T>,
) -> impl Future<Output = T> {
    TURBO_TASKS.scope(
        tt,
        CURRENT_TASK_ID.scope(current_task, PREVIOUS_CELLS.scope(Default::default(), f)),
    )
}

/// Get an [Invalidator] that can be used to invalidate the current [Task]
/// based on external events.
pub fn get_invalidator() -> Invalidator {
    let handle = tokio::runtime::Handle::current();
    Invalidator {
        task: current_task("turbo_tasks::get_invalidator()"),
        turbo_tasks: weak_turbo_tasks(),
        handle,
    }
}

pub async fn spawn_blocking<T: Send + 'static>(func: impl FnOnce() -> T + Send + 'static) -> T {
    let (r, d) = tokio::task::spawn_blocking(|| {
        let start = Instant::now();
        let r = func();
        (r, start.elapsed())
    })
    .await
    .unwrap();
    timed_future::add_duration(d);
    r
}

pub fn spawn_thread(func: impl FnOnce() -> () + Send + 'static) {
    let handle = tokio::runtime::Handle::current();
    thread::spawn(move || {
        let guard = handle.enter();
        func();
        drop(guard);
    });
}

pub(crate) async fn read_task_output(
    this: &dyn TurboTasksApi,
    id: TaskId,
    strongly_consistent: bool,
) -> Result<RawVc> {
    loop {
        match this.try_read_task_output(id, strongly_consistent)? {
            Ok(result) => return Ok(result),
            Err(listener) => listener.await,
        }
    }
}

pub(crate) async unsafe fn read_task_output_untracked(
    this: &dyn TurboTasksApi,
    id: TaskId,
    strongly_consistent: bool,
) -> Result<RawVc> {
    loop {
        match unsafe { this.try_read_task_output_untracked(id, strongly_consistent) }? {
            Ok(result) => return Ok(result),
            Err(listener) => listener.await,
        }
    }
}

pub(crate) async fn read_task_cell(
    this: &dyn TurboTasksApi,
    id: TaskId,
    index: usize,
) -> Result<CellContent> {
    loop {
        match this.try_read_task_cell(id, index)? {
            Ok(result) => return Ok(result),
            Err(listener) => listener.await,
        }
    }
}

pub(crate) async unsafe fn read_task_cell_untracked(
    this: &dyn TurboTasksApi,
    id: TaskId,
    index: usize,
) -> Result<CellContent> {
    loop {
        match unsafe { this.try_read_task_cell_untracked(id, index) }? {
            Ok(result) => return Ok(result),
            Err(listener) => listener.await,
        }
    }
}

pub struct CurrentCellRef {
    current_task: TaskId,
    index: usize,
    type_id: ValueTypeId,
}

impl CurrentCellRef {
    pub fn conditional_update_shared<
        T: Send + Sync + 'static,
        F: FnOnce(Option<&T>) -> Option<T>,
    >(
        &self,
        functor: F,
    ) {
        let tt = turbo_tasks();
        let content = tt
            .read_current_task_cell(self.index)
            .ok()
            .and_then(|v| v.try_cast::<T>());
        let update = functor(content.as_ref().map(|read| &**read));
        if let Some(update) = update {
            tt.update_current_task_cell(
                self.index,
                CellContent(Some(SharedReference(Some(self.type_id), Arc::new(update)))),
            )
        }
    }

    pub fn compare_and_update_shared<T: PartialEq + Send + Sync + 'static>(&self, new_content: T) {
        self.conditional_update_shared(|old_content| {
            if let Some(old_content) = old_content {
                if PartialEq::eq(&new_content, old_content) {
                    return None;
                }
            }
            Some(new_content)
        });
    }

    pub fn update_shared<T: Send + Sync + 'static>(&self, new_content: T) {
        let tt = turbo_tasks();
        tt.update_current_task_cell(
            self.index,
            CellContent(Some(SharedReference(
                Some(self.type_id),
                Arc::new(new_content),
            ))),
        )
    }
}

impl From<CurrentCellRef> for RawVc {
    fn from(cell: CurrentCellRef) -> Self {
        RawVc::TaskCell(cell.current_task, cell.index)
    }
}

pub fn find_cell_by_key<
    K: Debug + Eq + Ord + Hash + Typed + TypedForInput + Send + Sync + 'static,
>(
    type_id: ValueTypeId,
    key: K,
) -> CurrentCellRef {
    PREVIOUS_CELLS.with(|c| {
        let current_task = current_task("cellting turbo_tasks values");
        let mut map = c.borrow_mut();
        let index = *map
            .by_key
            .entry((
                type_id,
                SharedValue(Some(K::get_value_type_id()), Arc::new(key)),
            ))
            .or_insert_with(|| with_turbo_tasks(|tt| tt.get_fresh_cell(current_task)));
        CurrentCellRef {
            current_task,
            index,
            type_id,
        }
    })
}

pub fn find_cell_by_type(type_id: ValueTypeId) -> CurrentCellRef {
    PREVIOUS_CELLS.with(|cell| {
        let current_task = current_task("cellting turbo_tasks values");
        let mut map = cell.borrow_mut();
        let (ref mut current_index, ref mut list) = map.by_type.entry(type_id).or_default();
        let index = if let Some(i) = list.get(*current_index) {
            *i
        } else {
            let index = turbo_tasks().get_fresh_cell(current_task);
            list.push(index);
            index
        };
        *current_index += 1;
        CurrentCellRef {
            current_task,
            index,
            type_id,
        }
    })
}
