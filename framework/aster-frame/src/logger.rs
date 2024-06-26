// SPDX-License-Identifier: MPL-2.0

use log::{Metadata, Record};

use crate::{config::DEFAULT_LOG_LEVEL, early_println};

const LOGGER: Logger = Logger {};

struct Logger {}

impl log::Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= DEFAULT_LOG_LEVEL
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            // early_println!("[{}]: {}", record.level(), record.args());
            early_println!(
                "[{}]: CPU#{}: {}",
                record.level(),
                crate::cpu::this_cpu(),
                record.args()
            );
        }
    }

    fn flush(&self) {}
}

pub(crate) fn init() {
    log::set_logger(&LOGGER)
        .map(|()| log::set_max_level(DEFAULT_LOG_LEVEL.to_level_filter()))
        .unwrap();
}
