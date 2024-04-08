// SPDX-License-Identifier: MPL-2.0

use core::fmt::Arguments;

use crate::sync::SpinLock;

pub static CONSOLE_LOCK: SpinLock<()> = SpinLock::new(());

pub fn print(args: Arguments) {
    let _guard = CONSOLE_LOCK.lock_irq_disabled();
    crate::arch::console::print(args);
}

#[macro_export]
macro_rules! early_print {
  ($fmt: literal $(, $($arg: tt)+)?) => {
    $crate::console::print(format_args!($fmt $(, $($arg)+)?))
  }
}

#[macro_export]
#[allow_internal_unstable(format_args_nl)]
macro_rules! early_println {
  ($fmt: literal $(, $($arg: tt)+)?) => {
    $crate::console::print(format_args_nl!($fmt $(, $($arg)+)?))
  }
}
