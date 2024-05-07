// SPDX-License-Identifier: MPL-2.0

mod spin_mutex;
mod wq_mutex;

pub use self::wq_mutex::{Mutex, MutexGuard};
