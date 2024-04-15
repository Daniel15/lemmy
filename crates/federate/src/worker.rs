use crate::{
  inboxes::CommunityInboxCollector,
  util::{
    get_activity_cached,
    get_actor_cached,
    get_latest_activity_id,
    WORK_FINISHED_RECHECK_DELAY,
  },
};
use activitypub_federation::{
  activity_sending::SendActivityTask,
  config::{Data, FederationConfig},
  protocol::context::WithContext,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Days, TimeZone, Utc};
use lemmy_api_common::{context::LemmyContext, federate_retry_sleep_duration};
use lemmy_apub::{activity_lists::SharedInboxActivities, FEDERATION_CONTEXT};
use lemmy_db_schema::{
  newtypes::ActivityId,
  source::{
    activity::SentActivity,
    federation_queue_state::FederationQueueState,
    instance::{Instance, InstanceForm},
  },
  utils::{naive_now, ActualDbPool, DbPool},
};
use once_cell::sync::Lazy;
use reqwest::Url;
use std::{
  collections::BinaryHeap,
  ops::{Add, Deref},
  time::Duration,
};
use tokio::{
  sync::mpsc::{self, UnboundedSender},
  time::sleep,
};
use tokio_util::sync::CancellationToken;

/// Save state to db after this time has passed since the last state (so if the server crashes or is SIGKILLed, less than X seconds of activities are resent)
static SAVE_STATE_EVERY_TIME: Duration = Duration::from_secs(60);

static CONCURRENT_SENDS: Lazy<i64> = Lazy::new(|| {
  std::env::var("LEMMY_FEDERATION_CONCURRENT_SENDS_PER_INSTANCE")
    .ok()
    .and_then(|s| s.parse().ok())
    .unwrap_or(8)
});
/// Maximum number of successful sends to allow out of order
const MAX_SUCCESSFULS: usize = 1000;

pub(crate) struct InstanceWorker {
  instance: Instance,
  stop: CancellationToken,
  config: FederationConfig<LemmyContext>,
  stats_sender: UnboundedSender<(String, FederationQueueState)>,
  state: FederationQueueState,
  last_state_insert: DateTime<Utc>,
  pool: ActualDbPool,
  inbox_collector: CommunityInboxCollector,
}

#[derive(Debug, PartialEq, Eq)]
struct SendSuccessInfo {
  activity_id: ActivityId,
  published: Option<DateTime<Utc>>,
  was_skipped: bool,
}
impl PartialOrd for SendSuccessInfo {
  fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
    other.activity_id.partial_cmp(&self.activity_id)
  }
}
impl Ord for SendSuccessInfo {
  fn cmp(&self, other: &Self) -> std::cmp::Ordering {
    other.activity_id.cmp(&self.activity_id)
  }
}
enum SendActivityResult {
  Success(SendSuccessInfo),
  Failure {
    fail_count: i32,
    // activity_id: ActivityId,
  },
}

impl InstanceWorker {
  pub(crate) async fn init_and_loop(
    instance: Instance,
    config: FederationConfig<LemmyContext>,
    stop: CancellationToken,
    stats_sender: UnboundedSender<(String, FederationQueueState)>,
  ) -> Result<(), anyhow::Error> {
    let pool = config.to_request_data().inner_pool().clone();
    let state = FederationQueueState::load(&mut DbPool::Pool(&pool), instance.id).await?;
    let mut worker = InstanceWorker {
      inbox_collector: CommunityInboxCollector::new(
        pool.clone(),
        instance.id,
        instance.domain.clone(),
      ),
      instance,
      stop,
      config,
      stats_sender,
      state,
      last_state_insert: Utc.timestamp_nanos(0),
      pool,
    };
    worker.loop_until_stopped().await
  }
  /// loop fetch new activities from db and send them to the inboxes of the given instances
  /// this worker only returns if (a) there is an internal error or (b) the cancellation token is cancelled (graceful exit)
  async fn loop_until_stopped(&mut self) -> Result<()> {
    self.initial_fail_sleep().await?;
    let mut latest_id = self.get_latest_id().await?;

    // activities that have been successfully sent but
    // that are not the lowest number and thus can't be written to the database yet
    let mut successfuls = BinaryHeap::<SendSuccessInfo>::new();
    // number of activities that currently have a task spawned to send it
    let mut in_flight: i64 = 0;

    // each HTTP send will report back to this channel concurrently
    let (report_send_result, mut receive_send_result) =
      tokio::sync::mpsc::unbounded_channel::<SendActivityResult>();
    while !self.stop.is_cancelled() {
      // check if we need to wait for a send to finish before sending the next one
      // we wait if (a) the last request failed, only if a request is already in flight (not at the start of the loop)
      // or (b) if we have too many successfuls in memory or (c) if we have too many in flight
      let need_wait_for_event = (in_flight != 0 && self.state.fail_count > 0)
        || successfuls.len() >= MAX_SUCCESSFULS
        || in_flight >= *CONCURRENT_SENDS;
      if need_wait_for_event || receive_send_result.len() > 4 {
        // if len() > 0 then this does not block and allows us to write to db more often
        // if len is 0 then this means we wait for something to change our above conditions,
        // which can only happen by an event sent into the channel
        self
          .handle_send_results(&mut receive_send_result, &mut successfuls, &mut in_flight)
          .await?;
        // handle_send_results does not guarantee that we are now in a condition where we want to  send a new one,
        // so repeat this check until the if no longer applies
        continue;
      } else {
        // send a new activity if there is one
        self.inbox_collector.update_communities().await?;
        let next_id = {
          // calculate next id to send based on the last id and the in flight requests
          let last_successful_id = self
            .state
            .last_successful_id
            .map(|e| e.0)
            .expect("set above");
          ActivityId(last_successful_id + (successfuls.len() as i64) + in_flight + 1)
        };
        if next_id > latest_id {
          // lazily fetch latest id only if we have cought up
          latest_id = self.get_latest_id().await?;
          if next_id > latest_id {
            // no more work to be done, wait before rechecking
            tokio::select! {
              () = sleep(*WORK_FINISHED_RECHECK_DELAY) => {},
              () = self.stop.cancelled() => {}
            }
            continue;
          }
        }
        in_flight += 1;
        self
          .spawn_send_if_needed(next_id, report_send_result.clone())
          .await?;
      }
    }
    // final update of state in db on shutdown
    self.save_and_send_state().await?;
    Ok(())
  }

  async fn initial_fail_sleep(&mut self) -> Result<()> {
    // before starting queue, sleep remaining duration if last request failed
    if self.state.fail_count > 0 {
      let last_retry = self
        .state
        .last_retry
        .context("impossible: if fail count set last retry also set")?;
      let elapsed = (Utc::now() - last_retry).to_std()?;
      let required = federate_retry_sleep_duration(self.state.fail_count);
      if elapsed >= required {
        return Ok(());
      }
      let remaining = required - elapsed;
      tracing::debug!(
        "{}: fail-sleeping for {:?} before starting queue",
        self.instance.domain,
        remaining
      );
      tokio::select! {
        () = sleep(remaining) => {},
        () = self.stop.cancelled() => {}
      }
    }
    Ok(())
  }

  /// get newest activity id and set it as last_successful_id if it's the first time this instance is seen
  async fn get_latest_id(&mut self) -> Result<ActivityId> {
    let latest_id = get_latest_activity_id(&mut self.pool()).await?;
    if self.state.last_successful_id.is_none() {
      // this is the initial creation (instance first seen) of the federation queue for this instance
      // skip all past activities:
      self.state.last_successful_id = Some(latest_id);
      // save here to ensure it's not read as 0 again later if no activities have happened
      self.save_and_send_state().await?;
    }
    Ok(latest_id)
  }

  async fn handle_send_results(
    &mut self,
    receive_inbox_result: &mut mpsc::UnboundedReceiver<SendActivityResult>,
    successfuls: &mut BinaryHeap<SendSuccessInfo>,
    in_flight: &mut i64,
  ) -> Result<(), anyhow::Error> {
    let mut force_write = false;
    let mut events = Vec::new();
    // wait for at least one event but if there's multiple handle them all
    receive_inbox_result.recv_many(&mut events, 1000).await;
    for event in events {
      match event {
        SendActivityResult::Success(s) => {
          self.state.fail_count = 0;
          *in_flight -= 1;
          if !s.was_skipped {
            self.mark_instance_alive().await?;
          }
          successfuls.push(s);
        }
        SendActivityResult::Failure { fail_count, .. } => {
          if fail_count > self.state.fail_count {
            // override fail count - if multiple activities are currently sending this value may get conflicting info but that's fine
            self.state.fail_count = fail_count;
            self.state.last_retry = Some(Utc::now());
            force_write = true;
          }
        }
      }
    }
    self
      .pop_successfuls_and_write(successfuls, force_write)
      .await?;
    Ok(())
  }
  async fn mark_instance_alive(&mut self) -> Result<()> {
    // Activity send successful, mark instance as alive if it hasn't been updated in a while.
    let updated = self.instance.updated.unwrap_or(self.instance.published);
    if updated.add(Days::new(1)) < Utc::now() {
      self.instance.updated = Some(Utc::now());

      let form = InstanceForm::builder()
        .domain(self.instance.domain.clone())
        .updated(Some(naive_now()))
        .build();
      Instance::update(&mut self.pool(), self.instance.id, form).await?;
    }
    Ok(())
  }
  /// Checks that sequential activities `last_successful_id + 1`, `last_successful_id + 2` etc have been sent successfully.
  /// In that case updates `last_successful_id` and saves the state to the database if the time since the last save is greater than `SAVE_STATE_EVERY_TIME`.
  async fn pop_successfuls_and_write(
    &mut self,
    successfuls: &mut BinaryHeap<SendSuccessInfo>,
    force_write: bool,
  ) -> Result<()> {
    let Some(mut last_id) = self.state.last_successful_id else {
      tracing::warn!("should be impossible: last successful id is None");
      return Ok(());
    };
    tracing::debug!(
      "last: {:?}, next: {:?}, currently in successfuls: {:?}",
      last_id,
      successfuls.peek(),
      successfuls.iter()
    );
    while successfuls
      .peek()
      .map(|a| &a.activity_id == &ActivityId(last_id.0 + 1))
      .unwrap_or(false)
    {
      let next = successfuls.pop().unwrap();
      last_id = next.activity_id;
      self.state.last_successful_id = Some(next.activity_id);
      self.state.last_successful_published_time = next.published;
    }

    let save_state_every = chrono::Duration::from_std(SAVE_STATE_EVERY_TIME).expect("not negative");
    if force_write || (Utc::now() - self.last_state_insert) > save_state_every {
      self.save_and_send_state().await?;
    }
    Ok(())
  }

  /// we collect the relevant inboxes in the main instance worker task, and only spawn the send task if we have inboxes to send to
  /// this limits CPU usage and reduces overhead for the (many) cases where we don't have any inboxes
  async fn spawn_send_if_needed(
    &mut self,
    activity_id: ActivityId,
    report: UnboundedSender<SendActivityResult>,
  ) -> Result<()> {
    let Some(ele) = get_activity_cached(&mut self.pool(), activity_id)
      .await
      .context("failed reading activity from db")?
    else {
      tracing::debug!("{}: {:?} does not exist", self.instance.domain, activity_id);
      report.send(SendActivityResult::Success(SendSuccessInfo {
        activity_id,
        published: None,
        was_skipped: true,
      }))?;
      return Ok(());
    };
    let activity = &ele.0;
    let inbox_urls = self
      .inbox_collector
      .get_inbox_urls(activity)
      .await
      .context("failed figuring out inbox urls")?;
    if inbox_urls.is_empty() {
      tracing::debug!("{}: {:?} no inboxes", self.instance.domain, activity.id);
      report.send(SendActivityResult::Success(SendSuccessInfo {
        activity_id,
        published: Some(activity.published),
        was_skipped: true,
      }))?;
      return Ok(());
    }
    let initial_fail_count = self.state.fail_count;
    let data = self.config.to_request_data();
    let stop = self.stop.clone();
    let domain = self.instance.domain.clone();
    tokio::spawn(async move {
      let mut report = report;
      let res = InstanceWorker::send_retry_loop(
        &ele.0,
        &ele.1,
        inbox_urls,
        &mut report,
        initial_fail_count,
        domain,
        data,
        stop,
      )
      .await;
      if let Err(e) = res {
        tracing::warn!(
          "sending {} errored internally, skipping activity: {:?}",
          ele.0.ap_id,
          e
        );
        report
          .send(SendActivityResult::Success(SendSuccessInfo {
            activity_id,
            published: None,
            was_skipped: true,
          }))
          .ok();
      }
    });
    Ok(())
  }

  // this function will return successfully when (a) send succeeded or (b) worker cancelled
  // and will return an error if an internal error occurred (send errors cause an infinite loop)
  async fn send_retry_loop(
    activity: &SentActivity,
    object: &SharedInboxActivities,
    inbox_urls: Vec<Url>,
    report: &mut UnboundedSender<SendActivityResult>,
    initial_fail_count: i32,
    domain: String,
    context: Data<LemmyContext>,
    stop: CancellationToken,
  ) -> Result<()> {
    debug_assert!(!inbox_urls.is_empty());

    let pool = &mut context.pool();
    let Some(actor_apub_id) = &activity.actor_apub_id else {
      return Err(anyhow::anyhow!("activity is from before lemmy 0.19"));
    };
    let actor = get_actor_cached(pool, activity.actor_type, actor_apub_id)
      .await
      .context("failed getting actor instance (was it marked deleted / removed?)")?;

    let object = WithContext::new(object.clone(), FEDERATION_CONTEXT.deref().clone());
    let requests = SendActivityTask::prepare(&object, actor.as_ref(), inbox_urls, &context).await?;
    for task in requests {
      // usually only one due to shared inbox
      tracing::debug!("sending out {}", task);
      let mut fail_count = initial_fail_count;
      while let Err(e) = task.sign_and_send(&context).await {
        fail_count += 1;
        report.send(SendActivityResult::Failure {
          fail_count,
          // activity_id: activity.id,
        })?;
        let retry_delay: Duration = federate_retry_sleep_duration(fail_count);
        tracing::info!(
          "{}: retrying {:?} attempt {} with delay {retry_delay:.2?}. ({e})",
          domain,
          activity.id,
          fail_count
        );
        tokio::select! {
          () = sleep(retry_delay) => {},
          () = stop.cancelled() => {
            // save state to db and exit
            // TODO: do we need to report state here to prevent hang on exit?
            return Ok(());
          }
        }
      }
    }
    report.send(SendActivityResult::Success(SendSuccessInfo {
      activity_id: activity.id,
      published: Some(activity.published),
      was_skipped: false,
    }))?;
    Ok(())
  }

  async fn save_and_send_state(&mut self) -> Result<()> {
    tracing::debug!("{}: saving and sending state", self.instance.domain);
    self.last_state_insert = Utc::now();
    FederationQueueState::upsert(&mut self.pool(), &self.state).await?;
    self
      .stats_sender
      .send((self.instance.domain.clone(), self.state.clone()))?;
    Ok(())
  }

  fn pool(&self) -> DbPool<'_> {
    DbPool::Pool(&self.pool)
  }
}
