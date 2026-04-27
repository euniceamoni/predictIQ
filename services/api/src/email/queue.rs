use anyhow::{Context, Result};
use redis::AsyncCommands;
use serde_json::Value;
use std::time::Duration;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::cache::RedisCache;
use crate::db::Database;
use crate::email::service::idempotency_key;
use crate::email::types::{EmailJobStatus, EmailJobType};
use crate::shutdown::ShutdownCoordinator;

const EMAIL_QUEUE_KEY: &str = "email:queue";
const EMAIL_PROCESSING_KEY: &str = "email:processing";
const EMAIL_RETRY_KEY: &str = "email:retry";
const EMAIL_DEAD_LETTER_KEY: &str = "email:dead_letter";

#[derive(Clone)]
pub struct EmailQueue {
    cache: RedisCache,
    db: Database,
}

impl EmailQueue {
    pub fn new(cache: RedisCache, db: Database) -> Self {
        Self { cache, db }
    }

    /// Enqueue a new email job
    pub async fn enqueue(
        &self,
        job_type: EmailJobType,
        recipient: &str,
        template_name: &str,
        template_data: Value,
        priority: i32,
    ) -> Result<Uuid> {
        let job_id = self
            .db
            .email_create_job(
                job_type.as_str(),
                recipient,
                template_name,
                template_data,
                priority,
            )
            .await?;

        // Add to Redis queue for processing
        let score = if priority > 0 {
            // Higher priority = lower score (processed first)
            -(priority as f64)
        } else {
            chrono::Utc::now().timestamp() as f64
        };

        let mut conn = self.cache.get_connection().await?;
        let _: () = conn.zadd(EMAIL_QUEUE_KEY, job_id.to_string(), score)
            .await
            .context("Failed to add job to queue")?;

        tracing::info!("Enqueued email job: {} for {}", job_id, recipient);
        Ok(job_id)
    }

    /// Dequeue the next job for processing
    pub async fn dequeue(&self) -> Result<Option<Uuid>> {
        let mut conn = self.cache.get_connection().await?;

        // Use ZPOPMIN to atomically get and remove the lowest score item
        let result: Option<(String, f64)> = conn
            .zpopmin(EMAIL_QUEUE_KEY, 1)
            .await
            .context("Failed to dequeue job")?;

        if let Some((job_id_str, _score)) = result {
            let job_id = Uuid::parse_str(&job_id_str)?;

            // Add to processing set
            let _: () = conn.sadd(EMAIL_PROCESSING_KEY, job_id.to_string())
                .await
                .context("Failed to mark job as processing")?;

            return Ok(Some(job_id));
        }

        Ok(None)
    }

    /// Mark a job as completed
    pub async fn mark_completed(&self, job_id: Uuid, message_id: Option<String>) -> Result<()> {
        self.db
            .email_update_job_status(job_id, EmailJobStatus::Completed.as_str(), None)
            .await?;

        // Remove from processing set
        let mut conn = self.cache.get_connection().await?;
        let _: () = conn.srem(EMAIL_PROCESSING_KEY, job_id.to_string())
            .await
            .context("Failed to remove from processing set")?;

        // Track sent event with real recipient
        if let Some(msg_id) = message_id {
            // Look up recipient from DB to avoid storing empty string
            let recipient = self
                .db
                .email_get_job(job_id)
                .await
                .ok()
                .flatten()
                .map(|j| j.recipient_email)
                .unwrap_or_default();
            self.db
                .email_create_event(Some(job_id), Some(&msg_id), "sent", &recipient, serde_json::json!({}))
                .await?;
        }

        tracing::info!("Marked email job as completed: {}", job_id);
        Ok(())
    }

    /// Mark a job as failed and schedule retry if attempts remain
    pub async fn mark_failed(&self, job_id: Uuid, error: &str) -> Result<()> {
        let job = self.db.email_get_job(job_id).await?;

        if let Some(job) = job {
            let new_attempts = job.attempts + 1;

            if new_attempts < job.max_attempts {
                // Schedule retry with exponential backoff
                let backoff_seconds = 2_u64.pow(new_attempts as u32) * 60; // 2min, 4min, 8min...
                let retry_at = chrono::Utc::now() + chrono::Duration::seconds(backoff_seconds as i64);

                self.db
                    .email_update_job_attempts(job_id, new_attempts, Some(error))
                    .await?;

                // Add to retry queue
                let mut conn = self.cache.get_connection().await?;
                let _: () = conn.zadd(
                    EMAIL_RETRY_KEY,
                    job_id.to_string(),
                    retry_at.timestamp() as f64,
                )
                .await
                .context("Failed to schedule retry")?;

                tracing::warn!(
                    "Email job {} failed (attempt {}/{}), retrying in {}s: {}",
                    job_id,
                    new_attempts,
                    job.max_attempts,
                    backoff_seconds,
                    error
                );
            } else {
                // Max attempts reached — mark as permanently failed and move to dead-letter set.
                self.db
                    .email_update_job_status(job_id, EmailJobStatus::Failed.as_str(), Some(error))
                    .await?;

                let mut conn = self.cache.get_connection().await?;
                let failed_at = chrono::Utc::now().timestamp() as f64;
                let _: () = conn
                    .zadd(EMAIL_DEAD_LETTER_KEY, job_id.to_string(), failed_at)
                    .await
                    .context("Failed to add job to dead-letter set")?;

                tracing::error!(
                    "Email job {} permanently failed after {} attempts: {}",
                    job_id,
                    new_attempts,
                    error
                );
            }

            // Remove from processing set
            let mut conn = self.cache.get_connection().await?;
            let _: () = conn.srem(EMAIL_PROCESSING_KEY, job_id.to_string())
                .await
                .context("Failed to remove from processing set")?;
        }

        Ok(())
    }

    /// Process retry queue - move jobs back to main queue if retry time has passed
    pub async fn process_retries(&self) -> Result<usize> {
        let mut conn = self.cache.get_connection().await?;
        let now = chrono::Utc::now().timestamp() as f64;

        // Get all jobs that are ready to retry
        let jobs: Vec<String> = conn
            .zrangebyscore(EMAIL_RETRY_KEY, "-inf", now)
            .await
            .context("Failed to get retry jobs")?;

        let count = jobs.len();

        for job_id_str in jobs {
            // Move back to main queue
            let job_id = Uuid::parse_str(&job_id_str)?;
            let _: () = conn.zadd(EMAIL_QUEUE_KEY, &job_id_str, now)
                .await
                .context("Failed to re-queue job")?;

            // Remove from retry queue
            let _: () = conn.zrem(EMAIL_RETRY_KEY, &job_id_str)
                .await
                .context("Failed to remove from retry queue")?;

            tracing::info!("Re-queued email job for retry: {}", job_id);
        }

        Ok(count)
    }

    /// List all job IDs currently in the dead-letter set (oldest-failed first).
    pub async fn list_dead_letter(&self) -> Result<Vec<Uuid>> {
        let mut conn = self.cache.get_connection().await?;
        let items: Vec<String> = conn
            .zrange(EMAIL_DEAD_LETTER_KEY, 0isize, -1isize)
            .await
            .context("Failed to list dead-letter set")?;

        items
            .iter()
            .map(|s| Uuid::parse_str(s).context("Invalid UUID in dead-letter set"))
            .collect()
    }

    /// Move a job from the dead-letter set back to the main queue for reprocessing.
    pub async fn requeue_dead_letter(&self, job_id: Uuid) -> Result<bool> {
        let mut conn = self.cache.get_connection().await?;

        let removed: usize = conn
            .zrem(EMAIL_DEAD_LETTER_KEY, job_id.to_string())
            .await
            .context("Failed to remove job from dead-letter set")?;

        if removed == 0 {
            return Ok(false);
        }

        // Reset DB status so the worker will pick it up again.
        self.db
            .email_update_job_status(job_id, crate::email::types::EmailJobStatus::Pending.as_str(), None)
            .await?;

        let score = chrono::Utc::now().timestamp() as f64;
        let _: () = conn
            .zadd(EMAIL_QUEUE_KEY, job_id.to_string(), score)
            .await
            .context("Failed to re-enqueue dead-letter job")?;

        tracing::info!("Requeued dead-letter email job: {}", job_id);
        Ok(true)
    }

    /// Get queue statistics
    pub async fn get_stats(&self) -> Result<QueueStats> {
        let mut conn = self.cache.get_connection().await?;

        let pending: usize = conn
            .zcard(EMAIL_QUEUE_KEY)
            .await
            .context("Failed to get queue size")?;

        let processing: usize = conn
            .scard(EMAIL_PROCESSING_KEY)
            .await
            .context("Failed to get processing count")?;

        let retry: usize = conn
            .zcard(EMAIL_RETRY_KEY)
            .await
            .context("Failed to get retry count")?;

        let dead_letter: usize = conn
            .zcard(EMAIL_DEAD_LETTER_KEY)
            .await
            .context("Failed to get dead-letter count")?;

        Ok(QueueStats {
            pending,
            processing,
            retry,
            dead_letter,
        })
    }

    /// Re-queue any jobs stuck in the processing set (e.g. from a previous crash)
    pub async fn recover_orphaned_jobs(&self) -> Result<usize> {
        let mut conn = self.cache.get_connection().await?;
        let stale: Vec<String> = conn
            .smembers(EMAIL_PROCESSING_KEY)
            .await
            .context("Failed to read processing set")?;

        let count = stale.len();
        for job_id_str in stale {
            let score = chrono::Utc::now().timestamp() as f64;
            let _: () = conn
                .zadd(EMAIL_QUEUE_KEY, &job_id_str, score)
                .await
                .context("Failed to re-queue orphaned job")?;
            let _: () = conn
                .srem(EMAIL_PROCESSING_KEY, &job_id_str)
                .await
                .context("Failed to remove orphaned job from processing set")?;
            tracing::warn!("Recovered orphaned email job: {}", job_id_str);
        }

        Ok(count)
    }

    /// Get the number of jobs currently being processed.
    pub async fn get_processing_count(&self) -> Result<usize> {
        let mut conn = self.cache.get_connection().await?;
        let count: usize = conn
            .scard(EMAIL_PROCESSING_KEY)
            .await
            .context("Failed to get processing count")?;
        Ok(count)
    }

    /// Background worker to process email queue.
    ///
    /// Accepts a [`CancellationToken`] and a [`ShutdownCoordinator`].
    /// On shutdown:
    ///   - stops dequeuing new jobs immediately
    ///   - allows any in-flight `process_job` call to complete
    ///   - calls `coordinator.worker_completed()` before returning
    pub async fn start_worker(
        &self,
        service: crate::email::EmailService,
        shutdown: CancellationToken,
        coordinator: ShutdownCoordinator,
    ) {
        tracing::info!("Email queue worker started");

        if let Err(e) = self.recover_orphaned_jobs().await {
            tracing::warn!("Failed to recover orphaned jobs: {}", e);
        }

        loop {
            // Do not pick up new work after shutdown signal.
            if shutdown.is_cancelled() {
                tracing::info!("Email queue worker: shutdown signal received, draining stops");
                break;
            }

            // Process retries first (quick Redis operation, safe to run).
            if let Err(e) = self.process_retries().await {
                tracing::error!("Error processing retries: {}", e);
            }

            match self.dequeue().await {
                Ok(Some(job_id)) => {
                    // In-flight job always runs to completion.
                    if let Err(e) = self.process_job(job_id, &service).await {
                        tracing::error!("Error processing job {}: {}", job_id, e);
                        let _ = self.mark_failed(job_id, &e.to_string()).await;
                    }
                }
                Ok(None) => {
                    // Queue empty — wait briefly or exit early on shutdown.
                    tokio::select! {
                        _ = sleep(Duration::from_secs(1)) => {}
                        _ = shutdown.cancelled() => {
                            tracing::info!("Email queue worker: shutdown during idle sleep, stopping");
                            break;
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Error dequeuing job: {}", e);
                    tokio::select! {
                        _ = sleep(Duration::from_secs(5)) => {}
                        _ = shutdown.cancelled() => {
                            tracing::info!("Email queue worker: shutdown during error backoff, stopping");
                            break;
                        }
                    }
                }
            }
        }

        tracing::info!("Email queue worker stopped");
        coordinator.worker_completed();
    }

    async fn process_job(&self, job_id: Uuid, service: &crate::email::EmailService) -> Result<()> {
        let job = self
            .db
            .email_get_job(job_id)
            .await?
            .context("Job not found")?;

        // Check if email is suppressed
        if self.db.email_is_suppressed(&job.recipient_email).await? {
            tracing::warn!(
                "Skipping email to suppressed address: {}",
                job.recipient_email
            );
            return self.mark_completed(job_id, None).await;
        }

        // Update status to processing
        self.db
            .email_update_job_status(job_id, EmailJobStatus::Processing.as_str(), None)
            .await?;

        // Derive a stable idempotency key for this job so retries never
        // produce duplicate sends within the configured TTL window.
        let idem = idempotency_key(
            &job.recipient_email,
            &job.template_name,
            &job.template_data,
        );

        // Send email (deduplication handled inside send_email_idempotent)
        let message_id = service
            .send_email_idempotent(
                &job.recipient_email,
                &job.template_name,
                &job.template_data,
                Some(&idem),
            )
            .await?;

        if message_id.starts_with("deduplicated:") {
            tracing::info!(
                job_id = %job_id,
                idem_key = %idem,
                "Email job skipped — already sent within idempotency window"
            );
        }

        // Mark as completed regardless (dedup counts as success)
        self.mark_completed(job_id, Some(message_id)).await?;

        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct QueueStats {
    pub pending: usize,
    pub processing: usize,
    pub retry: usize,
    pub dead_letter: usize,
}
