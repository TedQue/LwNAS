use std::path::Path;

use chrono::prelude::*;
use log::{LevelFilter, Log, Metadata, Record};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::{logger, targets};

/// Logger holds a lock protected inner buffer, when a logging record is received,
/// the inner buffer will be locked, formatted and then push to all targets
///
#[derive(Debug)]
struct Logger {
    time_fmt: String,
    max_level: LevelFilter,
    tx: mpsc::Sender<Option<String>>,
}

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= self.max_level
    }

    fn log(&self, record: &Record<'_>) {
        if self.enabled(record.metadata()) {
            let now: DateTime<Local> = Local::now();
            let msg = format!(
                "{} - {} - {}\n",
                now.format(&self.time_fmt),
                record.level(),
                record.args()
            );

            self.tx.try_send(Some(msg)).unwrap_or_else(|_| {
                eprintln!("async logger overflow");
            });
        }
    }

    fn flush(&self) {}
}

/// `Builder` acts as builder for initializing a `Logger`.
///
/// It can be used to set log levels, time formatting string, add log targets
/// like stderr, stdout and rotated files, that's all.
/// For this crate, there isn't mush "configures" to make.
///
#[derive(Debug)]
pub struct Builder {
    bound: usize,
    time_fmt: String,
    max_level: LevelFilter,
    targets: Vec<Box<dyn targets::Target>>,
}

impl Builder {
    /// Initializes the log builder with default log level `log::LevelFilter::Error`
    /// and default time formatting string `%d/%m/%Y %H:%M:%S`
    pub fn new() -> Self {
        Builder {
            bound: 1024,
            time_fmt: String::from("%d/%m/%Y %H:%M:%S"),
            max_level: LevelFilter::Error,
            targets: Vec::new(),
        }
    }

    pub fn bound(&mut self, new_bound: usize) -> &mut Self {
        self.bound = new_bound;
        self
    }

    /// Set initial log level that will be pass to Logger later
    pub fn max_level(&mut self, new_max_level: LevelFilter) -> &mut Self {
        self.max_level = new_max_level;
        self
    }

    /// Set time formatting string.
    /// Formatting is done via the format method, which format is equivalent to the familiar strftime format.
    /// See [format::strftime][format-strftime-url] documentation for full syntax and list of specifiers.
    ///
    /// [format-strftime-url]: https://docs.rs/chrono/latest/chrono/format/strftime/index.html#specifiers
    pub fn time_fmt(&mut self, fmt: &str) -> &mut Self {
        self.time_fmt.clear();
        self.time_fmt.push_str(fmt);
        self
    }

    /// Add stdout target
    pub fn stdout(&mut self, enabled: bool) -> &mut Self {
        if enabled {
            self.targets
                .push(Box::new(targets::WriteTarget::new(std::io::stdout())));
        }
        self
    }

    /// Add stderr target
    pub fn stderr(&mut self, enabled: bool) -> &mut Self {
        if enabled {
            self.targets
                .push(Box::new(targets::WriteTarget::new(std::io::stderr())));
        }
        self
    }

    /// Add rotated file target, up to `max_count` files with `max_size` bytes each.
    /// Say we set `base` = "demo.log", `max_size` = 5 * 1024, `max_count` = 3,
    /// then we can find 3 rotated files on disk, with name "demo.log.0", "demo.log.1"
    /// and "demo.log.2", size of 5KB at the most each.
    pub fn rotated_file<P: AsRef<Path>>(
        &mut self,
        base: P,
        max_size: u64,
        max_count: usize,
    ) -> &mut Self {
        if base.as_ref().to_str() != Some("") {
            self.targets.push(Box::new(targets::RotatedFileTarget::new(
                base, max_size, max_count,
            )));
        }
        self
    }

    /// Make and install the only global logger
    ///
    /// # Panics
    ///
    /// This function will panic if it is called more than once, or if another
    /// library has already initialized a global logger.
    pub fn setup(&mut self) -> AsyncLogTaskHolder {
        let (tx, rx) = mpsc::channel(self.bound);
        let boxed_logger = Box::new(logger::Logger {
            time_fmt: self.time_fmt.clone(),
            max_level: self.max_level,
            tx: tx.clone(),
        });

        // 设置全局 logger
        log::set_logger(Box::leak(boxed_logger)).unwrap();
        log::set_max_level(self.max_level);

        // 创建 log task 以消费 log message, targets move to async task
        AsyncLogTaskHolder {
            handle: tokio::spawn(async_log_task(rx, self.targets.drain(..).collect())),
            // handle: tokio::spawn(async_log_task(rx, std::mem::take(&mut self.targets))),
            tx: tx.clone(),
        }
    }
}

// RAII hold Async task
#[derive(Debug)]
pub struct AsyncLogTaskHolder {
    handle: JoinHandle<()>,
    tx: mpsc::Sender<Option<String>>,
}

impl AsyncLogTaskHolder {
    pub async fn shutdown(self) {
        // 发送 None 作为退出信号
        let _ = self.tx.send(None).await;

        // 等待 async logger task 结束
        let _ = self.handle.await;
    }
}

async fn async_log_task(
    mut rx: mpsc::Receiver<Option<String>>,
    mut targets: Vec<Box<dyn targets::Target>>,
) {
    while let Some(msg) = rx.recv().await {
        if let Some(s) = msg {
            for target in &mut targets {
                target.append(s.as_bytes());
            }
        } else {
            for target in &mut targets {
                target.flush();
            }
            break;
        }
    }
}
