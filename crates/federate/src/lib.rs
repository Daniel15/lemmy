use crate::{util::CancellableTask, worker::InstanceWorker};
use activitypub_federation::config::FederationConfig;
use lemmy_api_common::context::LemmyContext;
use lemmy_db_schema::{
  newtypes::InstanceId,
  source::{federation_queue_state::FederationQueueState, instance::Instance},
};
use lemmy_utils::error::LemmyResult;
use stats::receive_print_stats;
use std::{collections::HashMap, time::Duration};
use tokio::{
  sync::mpsc::{unbounded_channel, UnboundedSender},
  task::JoinHandle,
  time::sleep,
};
use tokio_util::sync::CancellationToken;
use tracing::info;

mod stats;
mod util;
mod worker;

static WORKER_EXIT_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(debug_assertions)]
static INSTANCES_RECHECK_DELAY: Duration = Duration::from_secs(5);
#[cfg(not(debug_assertions))]
static INSTANCES_RECHECK_DELAY: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct Opts {
  /// how many processes you are starting in total
  pub process_count: i32,
  /// the index of this process (1-based: 1 - process_count)
  pub process_index: i32,
}

pub struct SendManager {
  opts: Opts,
  workers: HashMap<InstanceId, CancellableTask>,
  context: FederationConfig<LemmyContext>,
  stats_sender: UnboundedSender<(String, FederationQueueState)>,
  exit_print: JoinHandle<()>,
}

impl SendManager {
  pub fn new(opts: Opts, context: FederationConfig<LemmyContext>) -> Self {
    assert!(opts.process_count > 0);
    assert!(opts.process_index > 0);

    let (stats_sender, stats_receiver) = unbounded_channel();
    Self {
      opts,
      workers: HashMap::new(),
      stats_sender,
      exit_print: tokio::spawn(receive_print_stats(
        context.inner_pool().clone(),
        stats_receiver,
      )),
      context,
    }
  }

  pub fn run(mut self) -> CancellableTask {
    let cancel = CancellableTask::spawn(WORKER_EXIT_TIMEOUT, move |cancel| async move {
      self.do_loop(cancel).await.unwrap();
      self.cancel().await.unwrap();
    });
    cancel
  }

  async fn do_loop(&mut self, cancel: CancellationToken) -> LemmyResult<()> {
    let process_index = self.opts.process_index - 1;
    info!(
      "Starting federation workers for process count {} and index {}",
      self.opts.process_count, process_index
    );
    let local_domain = self.context.settings().get_hostname_without_port()?;
    let mut pool = self.context.pool();
    loop {
      let mut total_count = 0;
      let mut dead_count = 0;
      let mut disallowed_count = 0;
      for (instance, allowed, is_dead) in
        Instance::read_federated_with_blocked_and_dead(&mut pool).await?
      {
        if instance.domain == local_domain {
          continue;
        }
        if instance.id.inner() % self.opts.process_count != process_index {
          continue;
        }
        total_count += 1;
        if !allowed {
          disallowed_count += 1;
        }
        if is_dead {
          dead_count += 1;
        }
        let should_federate = allowed && !is_dead;
        if should_federate {
          if self.workers.contains_key(&instance.id) {
            // worker already running
            continue;
          }
          // create new worker
          let instance = instance.clone();
          let req_data = self.context.to_request_data();
          let stats_sender = self.stats_sender.clone();
          self.workers.insert(
            instance.id,
            CancellableTask::spawn(WORKER_EXIT_TIMEOUT, move |stop| async move {
              InstanceWorker::init_and_loop(instance, req_data, stop, stats_sender).await
            }),
          );
        } else if !should_federate {
          if let Some(worker) = self.workers.remove(&instance.id) {
            if let Err(e) = worker.cancel().await {
              tracing::error!("error stopping worker: {e}");
            }
          }
        }
      }
      let worker_count = self.workers.len();
      tracing::info!("Federating to {worker_count}/{total_count} instances ({dead_count} dead, {disallowed_count} disallowed)");
      tokio::select! {
        () = sleep(INSTANCES_RECHECK_DELAY) => {},
        _ = cancel.cancelled() => { return Ok(()) }
      }
    }
  }

  pub async fn cancel(self) -> LemmyResult<()> {
    drop(self.stats_sender);
    tracing::warn!(
      "Waiting for {} workers ({:.2?} max)",
      self.workers.len(),
      WORKER_EXIT_TIMEOUT
    );
    // the cancel futures need to be awaited concurrently for the shutdown processes to be triggered concurrently
    futures::future::join_all(
      self
        .workers
        .into_values()
        .map(util::CancellableTask::cancel),
    )
    .await;
    self.exit_print.await?;
    Ok(())
  }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[allow(clippy::indexing_slicing)]
mod test {

  use super::*;
  use activitypub_federation::config::Data;
  use lemmy_utils::error::LemmyError;
  use serial_test::serial;
  use std::sync::{Arc, Mutex};
  use tokio::{spawn, time::sleep};

  struct TestData {
    send_manager: SendManager,
    context: Data<LemmyContext>,
    instances: Vec<Instance>,
  }
  impl TestData {
    async fn init(process_count: i32, process_index: i32) -> LemmyResult<Self> {
      let context = LemmyContext::init_test_context().await;
      let opts = Opts {
        process_count,
        process_index,
      };
      let federation_config = FederationConfig::builder()
        .domain("local.com")
        .app_data(context.clone())
        .build()
        .await?;

      let pool = &mut context.pool();
      let instances = vec![
        Instance::read_or_create(pool, "alpha.com".to_string()).await?,
        Instance::read_or_create(pool, "beta.com".to_string()).await?,
        Instance::read_or_create(pool, "gamma.com".to_string()).await?,
      ];

      let send_manager = SendManager::new(opts, federation_config);
      Ok(Self {
        send_manager,
        context,
        instances,
      })
    }

    async fn run(&mut self) -> LemmyResult<()> {
      // start it and cancel after workers are running
      let cancel = CancellationToken::new();
      let cancel_ = cancel.clone();
      spawn(async move {
        sleep(Duration::from_millis(100)).await;
        cancel_.cancel();
      });
      self.send_manager.do_loop(cancel.clone()).await?;
      Ok(())
    }

    async fn cleanup(self) -> LemmyResult<()> {
      self.send_manager.cancel().await?;
      Instance::delete_all(&mut self.context.pool()).await?;
      Ok(())
    }
  }

  // check that correct number of instance workers was started
  // TODO: need to wrap in Arc or something similar
  // TODO: test with different `opts`, dead/blocked instances etc

  #[tokio::test]
  #[serial]
  async fn test_send_manager() -> LemmyResult<()> {
    let mut data = TestData::init(1, 1).await?;

    data.run().await?;
    assert_eq!(3, data.send_manager.workers.len());

    data.cleanup().await?;
    Ok(())
  }

  #[tokio::test]
  #[serial]
  async fn test_send_manager_processes() -> LemmyResult<()> {
    let active = Arc::new(Mutex::new(vec![]));
    let execute = |count, index, len, active: Arc<Mutex<Vec<InstanceId>>>| async move {
      let mut data = TestData::init(count, index).await?;
      data.run().await?;
      assert_eq!(len, data.send_manager.workers.len());
      for k in data.send_manager.workers.keys() {
        active.lock().unwrap().push(*k);
      }
      data.cleanup().await?;
      Ok::<(), LemmyError>(())
    };
    execute(3, 1, 1, active.clone()).await?;
    execute(3, 2, 1, active.clone()).await?;
    execute(3, 3, 1, active.clone()).await?;
    execute(3, 4, 0, active.clone()).await?;
    execute(3, 6, 0, active.clone()).await?;

    // Should run exactly three workers
    assert_eq!(3, active.lock().unwrap().len());

    Ok(())
  }
}
