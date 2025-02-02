// Copyright (c) 2017 Stefan Lankes, RWTH Aachen University
//                    Colin Finck, RWTH Aachen University
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

#![allow(dead_code)]

use arch::x86_64::kernel::acpi;
use arch::x86_64::kernel::idt;
use arch::x86_64::kernel::irq;
use arch::x86_64::kernel::pic;
use arch::x86_64::kernel::pit;
use arch::x86_64::kernel::KERNEL_HEADER;
use core::sync::atomic::spin_loop_hint;
use core::{fmt, ptr, u32};
use environment;
use x86::controlregs::*;
use x86::cpuid::*;
use x86::msr::*;
use x86::time::*;

const IA32_MISC_ENABLE_ENHANCED_SPEEDSTEP: u64 = 1 << 16;
const IA32_MISC_ENABLE_SPEEDSTEP_LOCK: u64 = 1 << 20;
const IA32_MISC_ENABLE_TURBO_DISABLE: u64 = 1 << 38;

// MSR EFER bits
const EFER_SCE: u64 = (1 << 0);
const EFER_LME: u64 = (1 << 8);
const EFER_LMA: u64 = (1 << 10);
const EFER_NXE: u64 = (1 << 11);
const EFER_SVME: u64 = (1 << 12);
const EFER_LMSLE: u64 = (1 << 13);
const EFER_FFXSR: u64 = (1 << 14);
const EFER_TCE: u64 = (1 << 15);

static mut CPU_FREQUENCY: CpuFrequency = CpuFrequency::new();
static mut CPU_SPEEDSTEP: CpuSpeedStep = CpuSpeedStep::new();
static mut PHYSICAL_ADDRESS_BITS: u8 = 0;
static mut LINEAR_ADDRESS_BITS: u8 = 0;
static mut MEASUREMENT_TIMER_TICKS: u64 = 0;
static mut SUPPORTS_1GIB_PAGES: bool = false;
static mut SUPPORTS_AVX: bool = false;
static mut SUPPORTS_RDRAND: bool = false;
static mut SUPPORTS_TSC_DEADLINE: bool = false;
static mut SUPPORTS_X2APIC: bool = false;
static mut SUPPORTS_XSAVE: bool = false;
static mut SUPPORTS_FSGS: bool = false;
static mut TIMESTAMP_FUNCTION: unsafe fn() -> u64 = get_timestamp_rdtsc;

#[repr(C, align(16))]
pub struct XSaveLegacyRegion {
	pub fpu_control_word: u16,
	pub fpu_status_word: u16,
	pub fpu_tag_word: u16,
	pub fpu_opcode: u16,
	pub fpu_instruction_pointer: u32,
	pub fpu_instruction_pointer_high_or_cs: u32,
	pub fpu_data_pointer: u32,
	pub fpu_data_pointer_high_or_ds: u32,
	pub mxcsr: u32,
	pub mxcsr_mask: u32,
	pub st_space: [u8; 8 * 16],
	pub xmm_space: [u8; 16 * 16],
	pub padding: [u8; 96],
}

#[repr(C)]
pub struct XSaveHeader {
	pub xstate_bv: u64,
	pub xcomp_bv: u64,
	pub reserved: [u64; 6],
}

#[repr(C)]
pub struct XSaveAVXState {
	pub ymmh_space: [u8; 16 * 16],
}

/// XSave Area for AMD Lightweight Profiling.
/// Refer to AMD Lightweight Profiling Specification (Publication No. 43724), Figure 7-1.
#[repr(C)]
pub struct XSaveLWPState {
	pub lwpcb_address: u64,
	pub flags: u32,
	pub buffer_head_offset: u32,
	pub buffer_base: u64,
	pub buffer_size: u32,
	pub filters: u32,
	pub saved_event_record: [u64; 4],
	pub event_counter: [u32; 16],
}

#[repr(C)]
pub struct XSaveBndregs {
	pub bound_registers: [u8; 4 * 16],
}

#[repr(C)]
pub struct XSaveBndcsr {
	pub bndcfgu_register: u64,
	pub bndstatus_register: u64,
}

#[repr(C, align(64))]
pub struct FPUState {
	pub legacy_region: XSaveLegacyRegion,
	pub header: XSaveHeader,
	pub avx_state: XSaveAVXState,
	pub lwp_state: XSaveLWPState,
	pub bndregs: XSaveBndregs,
	pub bndcsr: XSaveBndcsr,
}

impl FPUState {
	pub const fn new() -> Self {
		Self {
			// Set FPU-related values to their default values after initialization.
			// Refer to Intel Vol. 3A, Table 9-1. IA-32 and Intel 64 Processor States Following Power-up, Reset, or INIT
			legacy_region: XSaveLegacyRegion {
				fpu_control_word: 0x37F,
				fpu_status_word: 0,
				fpu_tag_word: 0xFFFF,
				fpu_opcode: 0,
				fpu_instruction_pointer: 0,
				fpu_instruction_pointer_high_or_cs: 0,
				fpu_data_pointer: 0,
				fpu_data_pointer_high_or_ds: 0,
				mxcsr: 0x1F80,
				mxcsr_mask: 0,
				st_space: [0; 8 * 16],
				xmm_space: [0; 16 * 16],
				padding: [0; 96],
			},

			header: XSaveHeader {
				xstate_bv: 0,
				xcomp_bv: 0,
				reserved: [0; 6],
			},
			avx_state: XSaveAVXState {
				ymmh_space: [0; 16 * 16],
			},
			lwp_state: XSaveLWPState {
				lwpcb_address: 0,
				flags: 0,
				buffer_head_offset: 0,
				buffer_base: 0,
				buffer_size: 0,
				filters: 0,
				saved_event_record: [0; 4],
				event_counter: [0; 16],
			},
			bndregs: XSaveBndregs {
				bound_registers: [0; 4 * 16],
			},
			bndcsr: XSaveBndcsr {
				bndcfgu_register: 0,
				bndstatus_register: 0,
			},
		}
	}

	pub fn restore(&self) {
		if supports_xsave() {
			let bitmask = u32::MAX;
			unsafe {
				asm!("xrstorq $0" :: "*m"(self as *const Self), "{eax}"(bitmask), "{edx}"(bitmask) :: "volatile");
			}
		} else {
			unsafe {
				asm!("fxrstor $0" :: "*m"(self as *const Self) :: "volatile");
			}
		}
	}

	pub fn save(&mut self) {
		if supports_xsave() {
			let bitmask: u32 = u32::MAX;
			unsafe {
				asm!("xsaveq $0" : "=*m"(self as *mut Self) : "{eax}"(bitmask), "{edx}"(bitmask) : "memory" : "volatile");
			}
		} else {
			unsafe {
				asm!("fxsave $0; fnclex" : "=*m"(self as *mut Self) :: "memory" : "volatile");
			}
		}
	}

	pub fn restore_common(&self) {
		unsafe {
			asm!("fxrstor $0" :: "*m"(self as *const Self) :: "volatile");
		}
	}
}

enum CpuFrequencySources {
	Invalid,
	CommandLine,
	CpuIdBrandString,
	Measurement,
	Hypervisor,
}

impl fmt::Display for CpuFrequencySources {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match &self {
			CpuFrequencySources::CommandLine => write!(f, "Command Line"),
			CpuFrequencySources::CpuIdBrandString => write!(f, "CPUID Brand String"),
			CpuFrequencySources::Measurement => write!(f, "Measurement"),
			CpuFrequencySources::Hypervisor => write!(f, "Hypervisor"),
			_ => panic!("Attempted to print an invalid CPU Frequency Source"),
		}
	}
}

struct CpuFrequency {
	mhz: u16,
	source: CpuFrequencySources,
}

impl CpuFrequency {
	const fn new() -> Self {
		CpuFrequency {
			mhz: 0,
			source: CpuFrequencySources::Invalid,
		}
	}

	unsafe fn detect_from_cmdline(&mut self) -> Result<(), ()> {
		let mhz = environment::get_command_line_cpu_frequency();
		if mhz > 0 {
			self.mhz = mhz;
			self.source = CpuFrequencySources::CommandLine;
			Ok(())
		} else {
			Err(())
		}
	}

	unsafe fn detect_from_cpuid_brand_string(&mut self, cpuid: &CpuId) -> Result<(), ()> {
		let extended_function_info = cpuid
			.get_extended_function_info()
			.expect("CPUID Extended Function Info not available!");
		let brand_string = extended_function_info
			.processor_brand_string()
			.expect("CPUID Brand String not available!");

		let ghz_find = brand_string.find("GHz");
		if ghz_find.is_some() {
			let index = ghz_find.unwrap() - 4;
			let thousand_char = brand_string.chars().nth(index).unwrap();
			let decimal_char = brand_string.chars().nth(index + 1).unwrap();
			let hundred_char = brand_string.chars().nth(index + 2).unwrap();
			let ten_char = brand_string.chars().nth(index + 3).unwrap();

			if let (Some(thousand), '.', Some(hundred), Some(ten)) = (
				thousand_char.to_digit(10),
				decimal_char,
				hundred_char.to_digit(10),
				ten_char.to_digit(10),
			) {
				self.mhz = (thousand * 1000 + hundred * 100 + ten * 10) as u16;
				self.source = CpuFrequencySources::CpuIdBrandString;
				return Ok(());
			}
		}

		Err(())
	}

	unsafe fn detect_from_hypervisor(&mut self) -> Result<(), ()> {
		let cpu_freq = ptr::read_volatile(&KERNEL_HEADER.cpu_freq);
		if cpu_freq > 0 {
			self.mhz = cpu_freq as u16;
			self.source = CpuFrequencySources::Hypervisor;
			return Ok(());
		}

		Err(())
	}

	extern "x86-interrupt" fn measure_frequency_timer_handler(
		_stack_frame: &mut irq::ExceptionStackFrame,
	) {
		unsafe {
			MEASUREMENT_TIMER_TICKS += 1;
		}
		pic::eoi(pit::PIT_INTERRUPT_NUMBER);
	}

	fn measure_frequency(&mut self) -> Result<(), ()> {
		// The PIC is not initialized for uhyve, so we cannot measure anything.
		if environment::is_uhyve() {
			return Err(());
		}

		// Measure the CPU frequency by counting 3 ticks of a 100Hz timer.
		let tick_count = 3;
		let measurement_frequency = 100;

		// Use the Programmable Interval Timer (PIT) for this measurement, which is the only
		// system timer with a known constant frequency.
		idt::set_gate(
			pit::PIT_INTERRUPT_NUMBER,
			Self::measure_frequency_timer_handler as usize,
			0,
		);
		pit::init(measurement_frequency);

		// Determine the current timer tick.
		// We are probably loading this value in the middle of a time slice.
		let first_tick = unsafe { ptr::read_volatile(&MEASUREMENT_TIMER_TICKS) };

		// Wait until the tick count changes.
		// As soon as it has done, we are at the start of a new time slice.
		let start_tick = loop {
			let tick = unsafe { ptr::read_volatile(&MEASUREMENT_TIMER_TICKS) };
			if tick != first_tick {
				break tick;
			}

			spin_loop_hint();
		};

		// Count the number of CPU cycles during 3 timer ticks.
		let start = get_timestamp();

		loop {
			let tick = unsafe { ptr::read_volatile(&MEASUREMENT_TIMER_TICKS) };
			if tick - start_tick >= tick_count {
				break;
			}

			spin_loop_hint();
		}

		let end = get_timestamp();

		// Deinitialize the PIT again.
		// Now we can calculate our CPU frequency and implement a constant frequency tick counter
		// using RDTSC timestamps.
		pit::deinit();

		// Calculate the CPU frequency out of this measurement.
		let cycle_count = end - start;
		self.mhz = (measurement_frequency * cycle_count / (1_000_000 * tick_count)) as u16;
		self.source = CpuFrequencySources::Measurement;
		Ok(())
	}

	unsafe fn detect(&mut self) {
		let cpuid = CpuId::new();
		self.detect_from_hypervisor()
			.or_else(|_e| self.detect_from_cmdline())
			.or_else(|_e| self.detect_from_cpuid_brand_string(&cpuid))
			.or_else(|_e| self.measure_frequency())
			.expect("Could not determine the processor frequency");
	}

	fn get(&self) -> u16 {
		self.mhz
	}
}

impl fmt::Display for CpuFrequency {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{} MHz (from {})", self.mhz, self.source)
	}
}

struct CpuFeaturePrinter {
	feature_info: FeatureInfo,
	extended_feature_info: ExtendedFeatures,
	extended_function_info: ExtendedFunctionInfo,
}

impl CpuFeaturePrinter {
	fn new(cpuid: &CpuId) -> Self {
		CpuFeaturePrinter {
			feature_info: cpuid
				.get_feature_info()
				.expect("CPUID Feature Info not available!"),
			extended_feature_info: cpuid
				.get_extended_feature_info()
				.expect("CPUID Extended Feature Info not available!"),
			extended_function_info: cpuid
				.get_extended_function_info()
				.expect("CPUID Extended Function Info not available!"),
		}
	}
}

impl fmt::Display for CpuFeaturePrinter {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		if self.feature_info.has_mmx() {
			write!(f, "MMX ")?;
		}
		if self.feature_info.has_sse() {
			write!(f, "SSE ")?;
		}
		if self.feature_info.has_sse2() {
			write!(f, "SSE2 ")?;
		}
		if self.feature_info.has_sse3() {
			write!(f, "SSE3 ")?;
		}
		if self.feature_info.has_ssse3() {
			write!(f, "SSSE3 ")?;
		}
		if self.feature_info.has_sse41() {
			write!(f, "SSE4.1 ")?;
		}
		if self.feature_info.has_sse42() {
			write!(f, "SSE4.2 ")?;
		}
		if self.feature_info.has_avx() {
			write!(f, "AVX ")?;
		}
		if self.feature_info.has_eist() {
			write!(f, "EIST ")?;
		}
		if self.feature_info.has_aesni() {
			write!(f, "AESNI ")?;
		}
		if self.feature_info.has_rdrand() {
			write!(f, "RDRAND ")?;
		}
		if self.feature_info.has_fma() {
			write!(f, "FMA ")?;
		}
		if self.feature_info.has_movbe() {
			write!(f, "MOVBE ")?;
		}
		if self.feature_info.has_mce() {
			write!(f, "MCE ")?;
		}
		if self.feature_info.has_fxsave_fxstor() {
			write!(f, "FXSR ")?;
		}
		if self.feature_info.has_xsave() {
			write!(f, "XSAVE ")?;
		}
		if self.feature_info.has_vmx() {
			write!(f, "VMX ")?;
		}
		if self.extended_function_info.has_rdtscp() {
			write!(f, "RDTSCP ")?;
		}
		if self.feature_info.has_monitor_mwait() {
			write!(f, "MWAIT ")?;
		}
		if self.feature_info.has_clflush() {
			write!(f, "CLFLUSH ")?;
		}
		if self.feature_info.has_dca() {
			write!(f, "DCA ")?;
		}
		if self.feature_info.has_tsc_deadline() {
			write!(f, "TSC-DEADLINE ")?;
		}

		if self.extended_feature_info.has_avx2() {
			write!(f, "AVX2 ")?;
		}
		if self.extended_feature_info.has_bmi1() {
			write!(f, "BMI1 ")?;
		}
		if self.extended_feature_info.has_bmi2() {
			write!(f, "BMI2 ")?;
		}
		if self.extended_feature_info.has_rtm() {
			write!(f, "RTM ")?;
		}
		if self.extended_feature_info.has_hle() {
			write!(f, "HLE ")?;
		}
		if self.extended_feature_info.has_mpx() {
			write!(f, "MPX ")?;
		}
		if self.extended_feature_info.has_fsgsbase() {
			write!(f, "FSGSBASE ")?;
		}

		Ok(())
	}
}

struct CpuSpeedStep {
	eist_available: bool,
	eist_enabled: bool,
	eist_locked: bool,
	energy_bias_preference: bool,
	max_pstate: u8,
	is_turbo_pstate: bool,
}

impl CpuSpeedStep {
	const fn new() -> Self {
		CpuSpeedStep {
			eist_available: false,
			eist_enabled: false,
			eist_locked: false,
			energy_bias_preference: false,
			max_pstate: 0,
			is_turbo_pstate: false,
		}
	}

	fn detect_features(&mut self, cpuid: &CpuId) {
		let feature_info = cpuid
			.get_feature_info()
			.expect("CPUID Feature Info not available!");

		self.eist_available = feature_info.has_eist();
		if !self.eist_available {
			return;
		}

		let misc = unsafe { rdmsr(IA32_MISC_ENABLE) };
		self.eist_enabled = (misc & IA32_MISC_ENABLE_ENHANCED_SPEEDSTEP) > 0;
		self.eist_locked = (misc & IA32_MISC_ENABLE_SPEEDSTEP_LOCK) > 0;
		if !self.eist_enabled || self.eist_locked {
			return;
		}

		self.max_pstate = (unsafe { rdmsr(MSR_PLATFORM_INFO) } >> 8) as u8;
		if (misc & IA32_MISC_ENABLE_TURBO_DISABLE) == 0 {
			let turbo_pstate = unsafe { rdmsr(MSR_TURBO_RATIO_LIMIT) } as u8;
			if turbo_pstate > self.max_pstate {
				self.max_pstate = turbo_pstate;
				self.is_turbo_pstate = true;
			}
		}

		if let Some(thermal_power_info) = cpuid.get_thermal_power_info() {
			self.energy_bias_preference = thermal_power_info.has_energy_bias_pref();
		}
	}

	fn configure(&self) {
		if !self.eist_available || !self.eist_enabled || self.eist_locked {
			return;
		}

		if self.energy_bias_preference {
			unsafe {
				wrmsr(IA32_ENERGY_PERF_BIAS, 0);
			}
		}

		let mut perf_ctl_mask = u64::from(self.max_pstate) << 8;
		if self.is_turbo_pstate {
			perf_ctl_mask |= 1 << 32;
		}

		unsafe {
			wrmsr(IA32_PERF_CTL, perf_ctl_mask);
		}
	}
}

impl fmt::Display for CpuSpeedStep {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		if self.eist_available {
			write!(f, "Available, ")?;

			if !self.eist_enabled {
				write!(f, "but disabled")?;
			} else if self.eist_locked {
				write!(f, "but locked")?;
			} else {
				write!(f, "enabled with maximum P-State {}", self.max_pstate)?;
				if self.is_turbo_pstate {
					write!(f, " (Turbo Mode)")?;
				}

				if self.energy_bias_preference {
					write!(f, ", disabled Performance/Energy Bias")?;
				}
			}
		} else {
			write!(f, "Not Available")?;
		}

		Ok(())
	}
}

pub fn detect_features() {
	// Detect CPU features
	let cpuid = CpuId::new();
	let feature_info = cpuid
		.get_feature_info()
		.expect("CPUID Feature Info not available!");
	let extended_feature_info = cpuid
		.get_extended_feature_info()
		.expect("CPUID Extended Feature Info not available!");
	let extended_function_info = cpuid
		.get_extended_function_info()
		.expect("CPUID Extended Function Info not available!");

	unsafe {
		PHYSICAL_ADDRESS_BITS = extended_function_info
			.physical_address_bits()
			.expect("CPUID Physical Address Bits not available!");
		LINEAR_ADDRESS_BITS = extended_function_info
			.linear_address_bits()
			.expect("CPUID Linear Address Bits not available!");
		SUPPORTS_1GIB_PAGES = extended_function_info.has_1gib_pages();
		SUPPORTS_AVX = feature_info.has_avx();
		SUPPORTS_RDRAND = feature_info.has_rdrand();
		SUPPORTS_TSC_DEADLINE = feature_info.has_tsc_deadline();
		SUPPORTS_X2APIC = feature_info.has_x2apic();
		SUPPORTS_XSAVE = feature_info.has_xsave();
		SUPPORTS_FSGS = extended_feature_info.has_fsgsbase();

		if extended_function_info.has_rdtscp() {
			TIMESTAMP_FUNCTION = get_timestamp_rdtscp;
		}

		CPU_SPEEDSTEP.detect_features(&cpuid);
	}
}

pub fn configure() {
	// setup MSR EFER
	unsafe {
		wrmsr(IA32_EFER, rdmsr(IA32_EFER) | EFER_LMA | EFER_SCE | EFER_NXE);
	}

	//
	// CR0 CONFIGURATION
	//
	let mut cr0 = unsafe { cr0() };

	// Enable the FPU.
	cr0.insert(Cr0::CR0_MONITOR_COPROCESSOR | Cr0::CR0_NUMERIC_ERROR);
	cr0.remove(Cr0::CR0_EMULATE_COPROCESSOR);

	// Call the IRQ7 handler on the first FPU access.
	cr0.insert(Cr0::CR0_TASK_SWITCHED);

	// Prevent writes to read-only pages in Ring 0.
	cr0.insert(Cr0::CR0_WRITE_PROTECT);

	// Enable caching.
	cr0.remove(Cr0::CR0_CACHE_DISABLE | Cr0::CR0_NOT_WRITE_THROUGH);

	unsafe {
		cr0_write(cr0);
	}

	//
	// CR4 CONFIGURATION
	//
	let mut cr4 = unsafe { cr4() };

	// Enable Machine Check Exceptions.
	// No need to check for support here, all x86-64 CPUs support it.
	cr4.insert(Cr4::CR4_ENABLE_MACHINE_CHECK);

	// Enable full SSE support and indicates that the OS saves SSE context using FXSR.
	// No need to check for support here, all x86-64 CPUs support at least SSE2.
	cr4.insert(Cr4::CR4_ENABLE_SSE | Cr4::CR4_UNMASKED_SSE);

	if supports_xsave() {
		// Indicate that the OS saves extended context (AVX, AVX2, MPX, etc.) using XSAVE.
		cr4.insert(Cr4::CR4_ENABLE_OS_XSAVE);
	}

	if supports_fsgs() {
		cr4.insert(Cr4::CR4_ENABLE_FSGSBASE);
	} else {
		error!("libhermit-rs requires the CPU feature FSGSBASE");
		loop {
			spin_loop_hint();
		}
	}

	unsafe {
		cr4_write(cr4);
	}

	//
	// XCR0 CONFIGURATION
	//
	if supports_xsave() {
		// Enable saving the context for all known vector extensions.
		// Must happen after CR4_ENABLE_OS_XSAVE has been set.
		let mut xcr0 = unsafe { xcr0() };
		xcr0.insert(Xcr0::XCR0_FPU_MMX_STATE | Xcr0::XCR0_SSE_STATE);

		if supports_avx() {
			xcr0.insert(Xcr0::XCR0_AVX_STATE);
		}

		unsafe {
			xcr0_write(xcr0);
		}
	}

	// Initialize the FS register, which is later used for Thread-Local Storage.
	writefs(0);

	//
	// ENHANCED INTEL SPEEDSTEP CONFIGURATION
	//
	unsafe {
		CPU_SPEEDSTEP.configure();
	}
}

pub fn detect_frequency() {
	unsafe {
		CPU_FREQUENCY.detect();
	}
}

pub fn print_information() {
	let cpuid = CpuId::new();
	let extended_function_info = cpuid
		.get_extended_function_info()
		.expect("CPUID Extended Function Info not available!");
	let brand_string = extended_function_info
		.processor_brand_string()
		.expect("CPUID Brand String not available!");
	let feature_printer = CpuFeaturePrinter::new(&cpuid);

	infoheader!(" CPU INFORMATION ");
	infoentry!("Model", brand_string);

	unsafe {
		infoentry!("Frequency", CPU_FREQUENCY);
		infoentry!("SpeedStep Technology", CPU_SPEEDSTEP);
	}

	infoentry!("Features", feature_printer);
	infoentry!(
		"Physical Address Width",
		"{} bits",
		get_physical_address_bits()
	);
	infoentry!("Linear Address Width", "{} bits", get_linear_address_bits());
	infoentry!(
		"Supports 1GiB Pages",
		if supports_1gib_pages() { "Yes" } else { "No" }
	);
	infofooter!();
}

pub fn generate_random_number() -> Option<u32> {
	if unsafe { SUPPORTS_RDRAND } {
		let value: u32;
		unsafe {
			asm!("rdrand $0" : "=r"(value) ::: "volatile");
		}
		Some(value)
	} else {
		None
	}
}

#[inline]
pub fn get_linear_address_bits() -> u8 {
	unsafe { LINEAR_ADDRESS_BITS }
}

#[inline]
pub fn get_physical_address_bits() -> u8 {
	unsafe { PHYSICAL_ADDRESS_BITS }
}

#[inline]
pub fn supports_1gib_pages() -> bool {
	unsafe { SUPPORTS_1GIB_PAGES }
}

#[inline]
pub fn supports_avx() -> bool {
	unsafe { SUPPORTS_AVX }
}

#[inline]
pub fn supports_tsc_deadline() -> bool {
	unsafe { SUPPORTS_TSC_DEADLINE }
}

#[inline]
pub fn supports_x2apic() -> bool {
	unsafe { SUPPORTS_X2APIC }
}

#[inline]
pub fn supports_xsave() -> bool {
	unsafe { SUPPORTS_XSAVE }
}

#[inline]
pub fn supports_fsgs() -> bool {
	unsafe { SUPPORTS_FSGS }
}

/// Search the most significant bit
#[inline(always)]
pub fn msb(value: u64) -> Option<u64> {
	if value > 0 {
		let ret: u64;
		unsafe {
			asm!("bsr $1, $0" : "=r"(ret) : "r"(value) : "cc" : "volatile");
		}
		Some(ret)
	} else {
		None
	}
}

/// The halt function stops the processor until the next interrupt arrives
pub fn halt() {
	unsafe {
		asm!("hlt" :::: "volatile");
	}
}

/// Shutdown the system
pub fn shutdown() -> ! {
	info!("Shutting down system");
	acpi::poweroff();

	loop {
		halt();
	}
}

pub fn get_timer_ticks() -> u64 {
	// We simulate a timer with a 1 microsecond resolution by taking the CPU timestamp
	// and dividing it by the CPU frequency in MHz.
	get_timestamp() / u64::from(get_frequency())
}

pub fn get_frequency() -> u16 {
	unsafe { CPU_FREQUENCY.get() }
}

#[inline]
pub fn readfs() -> usize {
	let val: u64;
	unsafe {
		asm!("rdfsbase $0" : "=r"(val) ::: "volatile");
	}
	val as usize
}

#[inline]
pub fn readgs() -> usize {
	let val: u64;
	unsafe {
		asm!("rdgsbase $0" : "=r"(val) ::: "volatile");
	}
	val as usize
}

#[inline]
pub fn writefs(fs: usize) {
	unsafe {
		asm!("wrfsbase $0" :: "r"(fs as u64) :: "volatile");
	}
}

#[inline]
pub fn writegs(gs: usize) {
	unsafe {
		asm!("wrgsbase $0" :: "r"(gs as u64) :: "volatile");
	}
}

#[inline]
pub fn get_timestamp() -> u64 {
	unsafe { TIMESTAMP_FUNCTION() }
}

#[inline]
unsafe fn get_timestamp_rdtsc() -> u64 {
	asm!("lfence" ::: "memory" : "volatile");
	let value = rdtsc();
	asm!("lfence" ::: "memory" : "volatile");
	value
}

#[inline]
unsafe fn get_timestamp_rdtscp() -> u64 {
	let value = rdtscp();
	asm!("lfence" ::: "memory" : "volatile");
	value
}

/// Delay execution by the given number of microseconds using busy-waiting.
#[inline]
pub fn udelay(usecs: u64) {
	let end = get_timestamp() + u64::from(get_frequency()) * usecs;
	while get_timestamp() < end {
		spin_loop_hint();
	}
}
