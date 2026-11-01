//! Article submission queue and worker pool
//!
//! This module implements a queue-based article submission system using flume.
//! Articles are validated minimally on submission, queued, and then processed
//! by background workers that perform comprehensive validation and storage.

use crate::Message;
use crate::auth::DynAuth;
use crate::config::Config;
use crate::notify::FanoutSlot;
use crate::storage::DynStorage;
use anyhow::Result;
use flume::{Receiver, Sender};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{Instrument, debug, error, info, info_span};

/// An article queued for processing
#[derive(Debug, Clone)]
pub struct QueuedArticle {
    /// The parsed message
    pub message: Message,
    /// Size of the original message in bytes
    pub size: u64,
    /// Whether this is a control message
    pub is_control: bool,
    /// Whether comprehensive validation has already been done
    pub already_validated: bool,
}

/// Article processing queue using flume MPMC
#[derive(Clone)]
pub struct ArticleQueue {
    sender: Sender<QueuedArticle>,
    receiver: Receiver<QueuedArticle>,
    fanout: FanoutSlot,
}

impl ArticleQueue {
    /// Create a new article queue with the specified capacity
    pub fn new(capacity: usize) -> Self {
        let (sender, receiver) = flume::bounded(capacity);
        Self {
            sender,
            receiver,
            fanout: FanoutSlot::new(),
        }
    }

    /// Install the live notification fanout. Later calls replace it.
    pub fn set_fanout(&self, fanout: Arc<dyn crate::notify::ArticleFanout>) {
        self.fanout.set(fanout);
    }

    /// Publish header notes for articles that were just stored.
    pub fn announce(&self, article: &Message, placed: &[(String, u64)]) {
        self.fanout.publish(article, placed);
    }

    /// Submit an article to the queue for processing
    ///
    /// Returns Ok(()) if the article was queued successfully,
    /// Err if the queue is full or closed.
    pub async fn submit(&self, article: QueuedArticle) -> Result<()> {
        self.sender
            .send_async(article)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to queue article: {e}"))
    }

    /// Get the receiver for worker tasks
    pub fn receiver(&self) -> Receiver<QueuedArticle> {
        self.receiver.clone()
    }

    /// Returns true if the queue is empty
    pub fn is_empty(&self) -> bool {
        self.sender.is_empty()
    }

    /// Returns the number of items in the queue
    pub fn len(&self) -> usize {
        self.sender.len()
    }
}

/// Article worker pool configuration
pub struct WorkerPool {
    queue: ArticleQueue,
    storage: DynStorage,
    auth: DynAuth,
    config: Arc<RwLock<Config>>,
    worker_count: usize,
}

impl WorkerPool {
    /// Create a new worker pool
    pub fn new(
        queue: ArticleQueue,
        storage: DynStorage,
        auth: DynAuth,
        config: Arc<RwLock<Config>>,
        worker_count: usize,
    ) -> Self {
        Self {
            queue,
            storage,
            auth,
            config,
            worker_count,
        }
    }

    /// Start all worker tasks
    pub async fn start(&self) -> Vec<tokio::task::JoinHandle<()>> {
        let mut handles = Vec::with_capacity(self.worker_count);

        for worker_id in 0..self.worker_count {
            let receiver = self.queue.receiver();
            let storage = self.storage.clone();
            let auth = self.auth.clone();
            let config = self.config.clone();
            let fanout = self.queue.fanout.clone();

            let handle = tokio::spawn(async move {
                worker_task(worker_id, receiver, storage, auth, config, fanout).await;
            });

            handles.push(handle);
        }

        info!(
            worker_count = self.worker_count,
            "Article processing workers started"
        );
        handles
    }
}

/// Worker task that processes articles from the queue
async fn worker_task(
    worker_id: usize,
    receiver: Receiver<QueuedArticle>,
    storage: DynStorage,
    auth: DynAuth,
    config: Arc<RwLock<Config>>,
    fanout: FanoutSlot,
) {
    debug!(worker_id = worker_id, "Article worker started");

    while let Ok(queued_article) = receiver.recv_async().await {
        let message_id = queued_article
            .message
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("Message-ID"))
            .map(|(_, v)| v.as_str())
            .unwrap_or("<unknown>");

        let span = info_span!(
            "queue.process",
            worker_id = worker_id,
            message_id = message_id,
            size_bytes = queued_article.size,
            is_control = queued_article.is_control,
            outcome = tracing::field::Empty,
        );

        async {
            let start = std::time::Instant::now();
            match process_article(&queued_article, &storage, &auth, &config, &fanout).await {
                Ok(()) => {
                    tracing::Span::current().record("outcome", "success");
                    debug!(duration_ms = start.elapsed().as_millis() as u64, "Article processed");
                }
                Err(e) => {
                    tracing::Span::current().record("outcome", "failed");
                    error!(error = %e, duration_ms = start.elapsed().as_millis() as u64, "Article processing failed");
                }
            }
        }
        .instrument(span)
        .await;
    }

    debug!(worker_id = worker_id, "Article worker stopped");
}

/// Process a single article: comprehensive validation and storage
async fn process_article(
    queued_article: &QueuedArticle,
    storage: &DynStorage,
    auth: &DynAuth,
    config: &Arc<RwLock<Config>>,
    fanout: &FanoutSlot,
) -> Result<()> {
    let article = &queued_article.message;

    // Handle control messages first
    if queued_article.is_control {
        let cfg_guard = config.read().await;
        if crate::control::handle_control(article, storage, auth, &cfg_guard).await? {
            debug!("Processed control message");
            return Ok(());
        }
    }

    // Perform comprehensive validation only if not already done
    if !queued_article.already_validated {
        let cfg_guard = config.read().await;

        // Create filter chain from configuration
        let filter_chain = match crate::filters::factory::create_filter_chain(&cfg_guard.filters) {
            Ok(chain) => chain,
            Err(e) => {
                error!("Failed to create filter chain: {}", e);
                // Fall back to default chain if configuration is invalid
                crate::filters::FilterChain::default()
            }
        };

        // Use the configured filter chain for validation
        crate::handlers::utils::validate_article_with_filters(
            storage,
            auth,
            &cfg_guard,
            article,
            queued_article.size,
            &filter_chain,
        )
        .await?;
        drop(cfg_guard);
    }

    // Store the article (check if it already exists to avoid duplicates)
    let message_id = article
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("Message-ID"))
        .map(|(_, v)| v.as_str())
        .unwrap_or("");

    if !message_id.is_empty() && storage.get_article_by_id(message_id).await?.is_some() {
        debug!("Article already exists, skipping storage");
        return Ok(());
    }

    let placed = storage.store_article(article).await?;
    fanout.publish(article, &placed);
    debug!("Article stored successfully");

    Ok(())
}
