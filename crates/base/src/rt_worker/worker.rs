use crate::deno_runtime::DenoRuntime;
use crate::rt_worker::utils::{get_event_metadata, parse_worker_conf};
use crate::rt_worker::worker_ctx::create_supervisor;
use crate::utils::send_event_if_event_worker_available;
use anyhow::{anyhow, Error};
use event_worker::events::{
    EventMetadata, ShutdownEvent, UncaughtExceptionEvent, WorkerEventWithMetadata, WorkerEvents,
};
use log::{debug, error};
use sb_core::conn_sync::ConnSync;
use sb_workers::context::{UserWorkerMsgs, WorkerContextInitOpts};
use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::net::UnixStream;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot::{Receiver, Sender};
use tokio::sync::{oneshot, watch, Notify};
use tokio::time::Instant;
use uuid::Uuid;

use super::rt;
use super::supervisor::CPUUsageMetrics;
use super::worker_pool::SupervisorPolicy;

#[derive(Clone)]
pub struct Worker {
    pub worker_boot_start_time: Instant,
    pub events_msg_tx: Option<UnboundedSender<WorkerEventWithMetadata>>,
    pub pool_msg_tx: Option<UnboundedSender<UserWorkerMsgs>>,
    pub cancel: Option<Arc<Notify>>,
    pub event_metadata: EventMetadata,
    pub worker_key: Option<Uuid>,
    pub supervisor_policy: Option<SupervisorPolicy>,
    pub thread_name: String,
}

pub type HandleCreationType = Pin<Box<dyn Future<Output = Result<WorkerEvents, Error>>>>;

pub trait WorkerHandler: Send {
    fn handle_error(&self, error: Error) -> Result<WorkerEvents, Error>;
    fn handle_creation(
        &self,
        created_rt: DenoRuntime,
        unix_stream_rx: UnboundedReceiver<(UnixStream, Option<watch::Receiver<ConnSync>>)>,
        termination_event_rx: Receiver<WorkerEvents>,
        maybe_cpu_metrics_tx: Option<UnboundedSender<CPUUsageMetrics>>,
    ) -> HandleCreationType;
    fn as_any(&self) -> &dyn Any;
}

impl Worker {
    pub fn new(init_opts: &WorkerContextInitOpts) -> Result<Self, Error> {
        let (worker_key, pool_msg_tx, events_msg_tx, cancel, thread_name) =
            parse_worker_conf(&init_opts.conf);
        let event_metadata = get_event_metadata(&init_opts.conf);

        let worker_boot_start_time = Instant::now();

        Ok(Self {
            supervisor_policy: None,
            worker_boot_start_time,
            events_msg_tx,
            pool_msg_tx,
            cancel,
            event_metadata,
            worker_key,
            thread_name,
        })
    }

    pub fn set_supervisor_policy(&mut self, supervisor_policy: Option<SupervisorPolicy>) {
        self.supervisor_policy = supervisor_policy;
    }

    pub fn start(
        &self,
        mut opts: WorkerContextInitOpts,
        unix_channel_rx: UnboundedReceiver<(UnixStream, Option<watch::Receiver<ConnSync>>)>,
        booter_signal: Sender<Result<(), Error>>,
    ) {
        let events_msg_tx = self.events_msg_tx.clone();
        let event_metadata = self.event_metadata.clone();
        let cancel = self.cancel.clone();
        let supervisor_policy = self.supervisor_policy.unwrap_or_default();
        let worker_key = self.worker_key;
        let pool_msg_tx = self.pool_msg_tx.clone();
        let timing = opts.timing.take();
        let method_cloner = self.clone();
        let is_user_worker = opts.conf.is_user_worker();

        drop({
            let _rt_guard = rt::WORKER_RT.enter();
            let local = tokio::task::LocalSet::new();

            local.spawn_local(async move {
                let (maybe_cpu_usage_metrics_tx, maybe_cpu_usage_metrics_rx) = is_user_worker
                    .then(|| unbounded_channel::<CPUUsageMetrics>())
                    .unzip();

                let result = match DenoRuntime::new(opts).await {
                    Ok(mut new_runtime) => {
                        let _ = booter_signal.send(Ok(()));

                        // CPU TIMER
                        let (termination_event_tx, termination_event_rx) =
                            oneshot::channel::<WorkerEvents>();

                        let _cpu_timer;

                        // TODO: Allow customization of supervisor
                        if is_user_worker {
                            // cputimer is returned from supervisor and assigned here to keep it in scope.
                            _cpu_timer = create_supervisor(
                                worker_key.unwrap_or(Uuid::nil()),
                                &mut new_runtime,
                                supervisor_policy,
                                termination_event_tx,
                                pool_msg_tx.clone(),
                                maybe_cpu_usage_metrics_rx,
                                cancel,
                                timing,
                            )?;
                        }

                        let data = method_cloner.handle_creation(
                            new_runtime,
                            unix_channel_rx,
                            termination_event_rx,
                            maybe_cpu_usage_metrics_tx,
                        );

                        data.await
                    }

                    Err(err) => {
                        let _ = booter_signal.send(Err(anyhow!("worker boot error")));
                        method_cloner.handle_error(err)
                    }
                };

                match result {
                    Ok(event) => {
                        match event {
                            WorkerEvents::Shutdown(ShutdownEvent { cpu_time_used, .. })
                            | WorkerEvents::UncaughtException(UncaughtExceptionEvent {
                                cpu_time_used,
                                ..
                            }) => {
                                debug!("CPU time used: {:?}ms", cpu_time_used);
                            }

                            _ => {}
                        };

                        send_event_if_event_worker_available(
                            events_msg_tx.clone(),
                            event,
                            event_metadata.clone(),
                        );
                    }
                    Err(err) => error!("unexpected worker error {}", err),
                };

                worker_key.and_then(|worker_key_unwrapped| {
                    pool_msg_tx.map(|tx| {
                        if let Err(err) = tx.send(UserWorkerMsgs::Shutdown(worker_key_unwrapped)) {
                            error!(
                                "failed to send the shutdown signal to user worker pool: {:?}",
                                err
                            );
                        }
                    })
                });

                Ok::<(), Error>(())
            })
        });
    }
}
