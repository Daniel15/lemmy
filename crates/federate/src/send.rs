use crate::util::get_actor_cached;
use activitypub_federation::{
  activity_sending::SendActivityTask,
  config::Data,
  protocol::context::WithContext,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use lemmy_api_common::{context::LemmyContext, federate_retry_sleep_duration};
use lemmy_apub::{activity_lists::SharedInboxActivities, FEDERATION_CONTEXT};
use lemmy_db_schema::{newtypes::ActivityId, source::activity::SentActivity};
use reqwest::Url;
use std::ops::Deref;
use tokio::{sync::mpsc::UnboundedSender, time::sleep};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Eq)]
pub(crate) struct SendSuccessInfo {
  pub activity_id: ActivityId,
  pub published: Option<DateTime<Utc>>,
  pub was_skipped: bool,
}
/// order backwards by activity_id for the binary heap in the worker
impl PartialEq for SendSuccessInfo {
  fn eq(&self, other: &Self) -> bool {
    self.activity_id == other.activity_id
  }
}
/// order backwards because the binary heap is a max heap, and we need the smallest element to be on top
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
pub(crate) enum SendActivityResult {
  Success(SendSuccessInfo),
  Failure {
    fail_count: i32,
    // activity_id: ActivityId,
  },
}

pub(crate) struct SendRetryTask<'a> {
  pub activity: &'a SentActivity,
  pub object: &'a SharedInboxActivities,
  /// must not be empty at this point
  pub inbox_urls: Vec<Url>,
  /// report to the main instance worker
  pub report: &'a mut UnboundedSender<SendActivityResult>,
  /// the first request will be sent immediately, but the next one will be delayed according to the
  /// number of previous fails + 1
  pub initial_fail_count: i32,
  /// for logging
  pub domain: String,
  pub context: Data<LemmyContext>,
  pub stop: CancellationToken,
}

impl<'a> SendRetryTask<'a> {
  // this function will return successfully when (a) send succeeded or (b) worker cancelled
  // and will return an error if an internal error occurred (send errors cause an infinite loop)
  pub async fn send_retry_loop(self) -> Result<()> {
    let SendRetryTask {
      activity,
      object,
      inbox_urls,
      report,
      initial_fail_count,
      domain,
      context,
      stop,
    } = self;
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
        let retry_delay = federate_retry_sleep_duration(fail_count);
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
}
