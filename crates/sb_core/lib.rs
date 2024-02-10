use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use deno_core::error::AnyError;
use deno_core::v8::IsolateHandle;
use deno_core::OpState;
use deno_core::{op2, JsRuntime};
use futures::task::AtomicWaker;
use futures::FutureExt;
use log::error;
use serde::Serialize;
use tokio::sync::oneshot;

pub mod auth_tokens;
pub mod cache;
pub mod cert;
pub mod conn_sync;
pub mod emit;
pub mod errors_rt;
pub mod external_memory;
pub mod file_fetcher;
pub mod http_start;
pub mod net;
pub mod permissions;
pub mod runtime;
pub mod transpiler;
pub mod util;

#[derive(Debug, Clone)]
pub struct WorkerMetricSource {
    handle: IsolateHandle,
    waker: Arc<AtomicWaker>,
}

impl From<&mut JsRuntime> for WorkerMetricSource {
    fn from(value: &mut JsRuntime) -> Self {
        Self::from_js_runtime(value)
    }
}

impl WorkerMetricSource {
    pub fn from_js_runtime(runtime: &mut JsRuntime) -> Self {
        let handle = runtime.v8_isolate().thread_safe_handle();
        let waker = {
            let state = runtime.op_state();
            let state_mut = state.borrow_mut();

            state_mut.waker.clone()
        };

        Self { handle, waker }
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeMetricSource {
    main: WorkerMetricSource,
    event: Option<WorkerMetricSource>,
}

impl RuntimeMetricSource {
    pub fn new(main: WorkerMetricSource, maybe_event: Option<WorkerMetricSource>) -> Self {
        Self {
            main,
            event: maybe_event,
        }
    }

    async fn get_heap_statistics(&mut self) -> RuntimeHeapStatistics {
        #[repr(C)]
        struct InterruptData {
            heap_tx: oneshot::Sender<WorkerHeapStatistics>,
        }

        extern "C" fn interrupt_fn(
            isolate: &mut deno_core::v8::Isolate,
            data: *mut std::ffi::c_void,
        ) {
            let arg = unsafe { Box::<InterruptData>::from_raw(data as *mut _) };
            let mut v8_heap_stats = deno_core::v8::HeapStatistics::default();
            let mut worker_heap_stats = WorkerHeapStatistics::default();

            isolate.get_heap_statistics(&mut v8_heap_stats);

            worker_heap_stats.total_heap_size = v8_heap_stats.total_heap_size();
            worker_heap_stats.total_heap_executable = v8_heap_stats.total_heap_size_executable();
            worker_heap_stats.total_physical_size = v8_heap_stats.total_physical_size();
            worker_heap_stats.total_available_size = v8_heap_stats.total_available_size();
            worker_heap_stats.total_global_handles_size = v8_heap_stats.total_global_handles_size();
            worker_heap_stats.used_global_handles_size = v8_heap_stats.used_global_handles_size();
            worker_heap_stats.used_heap_size = v8_heap_stats.used_heap_size();
            worker_heap_stats.malloced_memory = v8_heap_stats.malloced_memory();
            worker_heap_stats.external_memory = v8_heap_stats.external_memory();
            worker_heap_stats.peak_malloced_memory = v8_heap_stats.peak_malloced_memory();

            if let Err(err) = arg.heap_tx.send(worker_heap_stats) {
                error!("failed to send worker heap statistics: {:?}", err);
            }
        }

        let request_heap_statistics_fn = |arg: Option<&mut WorkerMetricSource>| {
            let Some(source) = arg else {
                return async { None::<WorkerHeapStatistics> }.boxed();
            };

            let (tx, rx) = oneshot::channel::<WorkerHeapStatistics>();
            let data_ptr_mut = Box::into_raw(Box::new(InterruptData { heap_tx: tx }));

            if !source
                .handle
                .request_interrupt(interrupt_fn, data_ptr_mut as *mut std::ffi::c_void)
            {
                drop(unsafe { Box::from_raw(data_ptr_mut) });
                return async { None }.boxed();
            }

            let waker = source.waker.clone();

            async move {
                waker.wake();
                rx.await.ok()
            }
            .boxed()
        };

        RuntimeHeapStatistics {
            main_worker_heap_stats: request_heap_statistics_fn(Some(&mut self.main))
                .await
                .unwrap_or_default(),

            event_worker_heap_stats: request_heap_statistics_fn(self.event.as_mut()).await,
        }
    }
}

#[derive(Debug, Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct WorkerHeapStatistics {
    total_heap_size: usize,
    total_heap_executable: usize,
    total_physical_size: usize,
    total_available_size: usize,
    total_global_handles_size: usize,
    used_global_handles_size: usize,
    used_heap_size: usize,
    malloced_memory: usize,
    external_memory: usize,
    peak_malloced_memory: usize,
}

#[derive(Debug, Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct RuntimeHeapStatistics {
    main_worker_heap_stats: WorkerHeapStatistics,
    event_worker_heap_stats: Option<WorkerHeapStatistics>,
}

#[derive(Debug, Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct RuntimeMetrics {
    #[serde(flatten)]
    heap_stats: RuntimeHeapStatistics,

    active_user_workers_count: usize,
    retired_user_workers_count: usize,
    received_requests_count: usize,
    handled_requests_count: usize,
}

#[op2(fast)]
fn op_is_terminal(state: &mut OpState, rid: u32) -> Result<bool, AnyError> {
    let handle = state.resource_table.get_handle(rid)?;
    Ok(handle.is_terminal())
}

#[op2(fast)]
fn op_stdin_set_raw(_state: &mut OpState, _is_raw: bool, _cbreak: bool) -> Result<(), AnyError> {
    Ok(())
}

#[op2(fast)]
fn op_console_size(_state: &mut OpState, #[buffer] _result: &mut [u32]) -> Result<(), AnyError> {
    Ok(())
}

#[op2(async)]
#[serde]
async fn op_runtime_metrics(state: Rc<RefCell<OpState>>) -> Result<RuntimeMetrics, AnyError> {
    let state = state.borrow();
    let mut runtime_metrics = RuntimeMetrics::default();

    runtime_metrics.heap_stats = state
        .borrow::<RuntimeMetricSource>()
        .clone()
        .get_heap_statistics()
        .await;

    Ok(runtime_metrics)
}

#[op2]
#[string]
pub fn op_read_line_prompt(
    #[string] _prompt_text: &str,
    #[string] _default_value: &str,
) -> Result<Option<String>, AnyError> {
    Ok(None)
}

#[op2(fast)]
fn op_set_exit_code(_state: &mut OpState, #[smi] _code: i32) -> Result<(), AnyError> {
    Ok(())
}

deno_core::extension!(
    sb_core_main_js,
    ops = [
        op_is_terminal,
        op_stdin_set_raw,
        op_console_size,
        op_read_line_prompt,
        op_set_exit_code,
        op_runtime_metrics
    ],
    esm_entry_point = "ext:sb_core_main_js/js/bootstrap.js",
    esm = [
        "js/permissions.js",
        "js/errors.js",
        "js/fieldUtils.js",
        "js/promises.js",
        "js/http.js",
        "js/denoOverrides.js",
        "js/navigator.js",
        "js/bootstrap.js",
        "js/main_worker.js",
    ]
);
