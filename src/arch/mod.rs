// Copyright (c) 2017 Stefan Lankes, RWTH Aachen University
//                    Colin Finck, RWTH Aachen University
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

// Platform-specific implementations
#[cfg(target_arch="aarch64")]
pub mod aarch64;

#[cfg(target_arch="x86_64")]
pub mod x86_64;

// Export our platform-specific modules.
#[cfg(target_arch="aarch64")]
pub use arch::aarch64::*;

#[cfg(target_arch="aarch64")]
pub use arch::aarch64::kernel::stubs::{switch,set_oneshot_timer,wakeup_core};

#[cfg(target_arch="aarch64")]
pub use arch::aarch64::kernel::{application_processor_init,boot_application_processors,
    network_adapter_init,output_message_byte,message_output_init,boot_processor_init,
    get_processor_count};

#[cfg(target_arch="aarch64")]
use arch::aarch64::kernel::percore::core_scheduler;

#[cfg(target_arch="aarch64")]
pub use arch::aarch64::kernel::percore;

#[cfg(target_arch="aarch64")]
pub use arch::aarch64::kernel::scheduler;

#[cfg(target_arch="aarch64")]
pub use arch::aarch64::kernel::processor;

#[cfg(target_arch="aarch64")]
pub use arch::aarch64::kernel::irq;

#[cfg(target_arch="aarch64")]
pub use arch::aarch64::kernel::systemtime::get_boot_time;

#[cfg(target_arch="x86_64")]
pub use arch::x86_64::*;

#[cfg(target_arch="x86_64")]
pub use arch::x86_64::kernel::{get_processor_count,application_processor_init,
	boot_application_processors,message_output_init,
	output_message_byte,boot_processor_init};
#[cfg(target_arch="x86_64")]
pub use arch::x86_64::kernel::apic::{set_oneshot_timer,wakeup_core};
#[cfg(target_arch="x86_64")]
pub use arch::x86_64::kernel::percore;
#[cfg(target_arch="x86_64")]
pub use arch::x86_64::kernel::processor;
#[cfg(target_arch="x86_64")]
pub use arch::x86_64::kernel::irq;
#[cfg(target_arch="x86_64")]
pub use arch::x86_64::kernel::scheduler;
#[cfg(target_arch="x86_64")]
pub use arch::x86_64::kernel::gdt::set_current_kernel_stack;
#[cfg(target_arch="x86_64")]
pub use arch::x86_64::kernel::systemtime::get_boot_time;
#[cfg(target_arch="x86_64")]
pub use arch::x86_64::kernel::switch::switch;