use chrono::Utc;
use lightning::util::logger::{Level, Logger};

pub struct NodeLogger {}

impl Logger for NodeLogger {
    fn log(&self, record: lightning::util::logger::Record) {
        let raw_log = record.args.to_string();
        let log = format!(
            "{} {:<5} [{}:{}] {}\n",
            Utc::now().format("%Y-%m-%d %H:%M:%S%.3f"),
            record.level.to_string(),
            record.module_path,
            record.line,
            raw_log
        );

        match record.level {
            Level::Gossip => log::trace!("{log}"),
            Level::Trace => log::trace!("{log}"),
            Level::Debug => log::debug!("{log}"),
            Level::Info => log::info!("{log}"),
            Level::Warn => log::warn!("{log}"),
            Level::Error => log::error!("{log}"),
        }
    }
}
