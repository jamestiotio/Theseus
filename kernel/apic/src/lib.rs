#![no_std]
#![feature(alloc)]

#![allow(dead_code)]

extern crate alloc;
#[macro_use] extern crate lazy_static;
#[macro_use] extern crate log;
extern crate volatile;
extern crate owning_ref;
extern crate irq_safety;
extern crate atomic_linked_list;
extern crate memory;
extern crate spin;
extern crate kernel_config;
extern crate raw_cpuid;
extern crate x86_64;
extern crate pit_clock;
extern crate atomic;


use core::ops::DerefMut;
use core::sync::atomic::{AtomicUsize, AtomicBool, Ordering, spin_loop_hint};
use volatile::{Volatile, ReadOnly, WriteOnly};
use alloc::boxed::Box;
use owning_ref::{BoxRef, BoxRefMut};
use spin::{RwLock, Once};
use raw_cpuid::CpuId;
use x86_64::registers::msr::*;
use irq_safety::hold_interrupts;
use memory::{FRAME_ALLOCATOR, Frame, ActivePageTable, PhysicalAddress, VirtualAddress, EntryFlags, MappedPages, allocate_pages};
use kernel_config::time::CONFIG_TIMESLICE_PERIOD_MICROSECONDS;
use atomic_linked_list::atomic_map::AtomicMap;
use atomic::Atomic;
use pit_clock::pit_wait;


pub static INTERRUPT_CHIP: Atomic<InterruptChip> = Atomic::new(InterruptChip::APIC);

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum InterruptChip {
    APIC,
    X2APIC,
    PIC,
}


lazy_static! {
    static ref LOCAL_APICS: AtomicMap<u8, RwLock<LocalApic>> = AtomicMap::new();
}

static APIC_REGS: Once<BoxRef<MappedPages, ApicRegisters>> = Once::new();

/// The processor id (from the ACPI MADT table) of the bootstrap processor
static BSP_PROCESSOR_ID: Once<u8> = Once::new(); 

pub fn get_bsp_id() -> Option<u8> {
    BSP_PROCESSOR_ID.try().cloned()
}

/// Returns true if the currently executing processor core is the bootstrap processor, 
/// i.e., the first procesor to run 
pub fn is_bsp() -> bool {
    rdmsr(IA32_APIC_BASE) & IA32_APIC_BASE_MSR_IS_BSP == IA32_APIC_BASE_MSR_IS_BSP
}

/// Returns true if the machine has support for x2apic
pub fn has_x2apic() -> bool {
    static IS_X2APIC: Once<bool> = Once::new(); // caches the result
    let res: &bool = IS_X2APIC.call_once( || {
        CpuId::new().get_feature_info().expect("Couldn't get CpuId feature info").has_x2apic()
    });
    *res // because call_once returns a reference to the cached IS_X2APIC value
}

/// Returns a reference to the list of LocalApics, one per processor core
pub fn get_lapics() -> &'static AtomicMap<u8, RwLock<LocalApic>> {
	&LOCAL_APICS
}


/// Returns the APIC ID of the currently executing processor core.
pub fn get_my_apic_id() -> Option<u8> {
    if has_x2apic() {
        // make sure this local apic is enabled in x2apic mode, otherwise we'll get a General Protection fault
        unsafe { wrmsr(IA32_APIC_BASE, rdmsr(IA32_APIC_BASE) | IA32_APIC_XAPIC_ENABLE | IA32_APIC_X2APIC_ENABLE); }
        let x2_id = rdmsr(IA32_X2APIC_APICID) as u32;
        Some(x2_id as u8)
    } else {
        APIC_REGS.try().map(|apic| {
            let raw = apic.lapic_id.read();
            (raw >> 24) as u8
        })
    }
}


/// Returns a reference to the LocalApic for the currently executing processsor core.
pub fn get_my_apic() -> Option<&'static RwLock<LocalApic>> {
    get_my_apic_id().and_then(|id| LOCAL_APICS.get(&id))
}


/// The possible destination shorthand values for IPI ICR.
/// 
/// See Intel manual Figure 10-28, Vol. 3A, 10-45. (PDF page 3079) 
pub enum LapicIpiDestination {
    /// Send IPI to a specific APIC 
    One(u8),
    /// Send IPI to my own (the current) APIC  
    Me,
    /// Send IPI to all APICs, including myself
    All,
    /// Send IPI to all APICs except for myself
    AllButMe,
}
impl LapicIpiDestination {
    /// Convert the enum to a bitmask value to be used in the interrupt command register
    pub fn as_icr_value(&self) -> u64 {
        match self {
            &LapicIpiDestination::One(apic_id) => { 
                if has_x2apic() {
                    (apic_id as u64) << 32
                } else {
                    (apic_id as u64) << 56
                }
            }
            &LapicIpiDestination::Me       => 0b01 << 18, // 0x4_0000
            &LapicIpiDestination::All      => 0b10 << 18, // 0x8_0000
            &LapicIpiDestination::AllButMe => 0b11 << 18, // 0xC_0000
        }
    }
}


/// Initially maps the base APIC MMIO register frames so that we can know which LAPIC (core) we are.
/// This only does something for apic/xapic systems, it does nothing for x2apic systems, as required.
pub fn init(active_table: &mut ActivePageTable) -> Result<(), &'static str> {
    let x2 = has_x2apic();
    let phys_addr = rdmsr(IA32_APIC_BASE) as PhysicalAddress;
    debug!("is x2apic? {}.  IA32_APIC_BASE (phys addr): {:#X}", x2, phys_addr);

    // x2apic doesn't require MMIO, it just uses MSRs instead, so we don't need to map the APIC registers.
    if !x2 {
        // offset into the apic_mapped_page is always 0, regardless of the physical address
        let apic_regs = BoxRef::new(Box::new(map_apic(active_table)?)).try_map(|mp| mp.as_type::<ApicRegisters>(0))?;
        APIC_REGS.call_once( || apic_regs);
    }

    Ok(())
}


/// return a mapping of APIC memory-mapped I/O registers 
fn map_apic(active_table: &mut ActivePageTable) -> Result<MappedPages, &'static str> {
    if has_x2apic() { return Err("map_apic() is only for use in apic/xapic systems, not x2apic."); }

    // make sure the local apic is enabled in xapic mode, otherwise we'll get a General Protection fault
    unsafe { wrmsr(IA32_APIC_BASE, rdmsr(IA32_APIC_BASE) | IA32_APIC_XAPIC_ENABLE); }
    
    let phys_addr = rdmsr(IA32_APIC_BASE) as PhysicalAddress;
    let new_page = try!(allocate_pages(1).ok_or("out of virtual address space!"));
    let frames = Frame::range_inclusive(Frame::containing_address(phys_addr), Frame::containing_address(phys_addr));
    let mut fa = try!(FRAME_ALLOCATOR.try().ok_or("apic::init(): couldn't get FRAME_ALLOCATOR")).lock();
    let apic_mapped_page = try!(active_table.map_allocated_pages_to(
        new_page, 
        frames, 
        EntryFlags::PRESENT | EntryFlags::WRITABLE | EntryFlags::NO_CACHE | EntryFlags::NO_EXECUTE, 
        fa.deref_mut())
    );

    Ok(apic_mapped_page)
}



pub const APIC_SPURIOUS_INTERRUPT_VECTOR: u32 = 0xFF; // as recommended by everyone on os dev wiki
const IA32_APIC_XAPIC_ENABLE: u64 = 1 << 11; // 0x800
const IA32_APIC_X2APIC_ENABLE: u64 = 1 << 10; // 0x400
const IA32_APIC_BASE_MSR_IS_BSP: u64 = 1 << 8; // 0x100
const APIC_SW_ENABLE: u32 = 1 << 8;
const APIC_TIMER_PERIODIC:  u32 = 0x2_0000;
const APIC_DISABLE: u32 = 0x1_0000;
const APIC_NMI: u32 = 4 << 8;



/// A structure that offers access to APIC/xAPIC through its I/O registers.
/// Definitions are based on [Intel's x86 Manual Vol 3a, Table 10-1]
/// (https://www.intel.com/content/dam/www/public/us/en/documents/manuals/64-ia-32-architectures-software-developer-vol-3a-part-1-manual.pdf). 
#[repr(C)]
pub struct ApicRegisters {
    _padding0:                        [u32; 8],
    /// This Lapic's ID. Some systems allow setting the ID, but it is typically read-only.
    /// Only the top 8 bits are relevant, so bit shift it to the right by 24 bits to get the actual ID.
    pub lapic_id:                     Volatile<u32>,         // 0x20
    _padding1:                        [u32; 3],
    pub lapic_version:                ReadOnly<u32>,         // 0x30
    _padding2:                        [u32; 3 + 4*4],
    pub task_priority:                Volatile<u32>,         // 0x80
    _padding3:                        [u32; 3],
    pub arbitration_priority:         ReadOnly<u32>,         // 0x90
    _padding4:                        [u32; 3],
    pub processor_priority:           ReadOnly<u32>,         // 0xA0
    _padding5:                        [u32; 3],
    pub eoi:                          WriteOnly<u32>,        // 0xB0
    _padding6:                        [u32; 3],
    pub remote_read:                  ReadOnly<u32>,         // 0xC0
    _padding7:                        [u32; 3],
    pub logical_destination:          Volatile<u32>,         // 0xD0
    _padding8:                        [u32; 3],
    pub destination_format:           Volatile<u32>,         // 0xE0
    _padding9:                        [u32; 3],
    pub spurious_interrupt_vector:    Volatile<u32>,         // 0xF0
    _padding10:                       [u32; 3],
    pub in_service_registers:         RegisterArray,         // 0x100
    pub trigger_mode_registers:       RegisterArray,         // 0x180
    pub interrupt_request_registers:  RegisterArray,         // 0x200
    pub error_status:                 ReadOnly<u32>,         // 0x280
    _padding11:                       [u32; 3 + 6*4],        
    pub lvt_cmci:                     Volatile<u32>,         // 0x2F0
    _padding12:                       [u32; 3],  
    pub interrupt_command_low:        Volatile<u32>,         // 0x300
    _padding13:                       [u32; 3],
    pub interrupt_command_high:       Volatile<u32>,         // 0x310
    _padding14:                       [u32; 3],
    pub lvt_timer:                    Volatile<u32>,         // 0x320
    _padding15:                       [u32; 3],
    pub lvt_thermal:                  Volatile<u32>,         // 0x330
    _padding16:                       [u32; 3],
    pub lvt_perf_monitor:             Volatile<u32>,         // 0x340
    _padding17:                       [u32; 3],
    pub lvt_lint0:                    Volatile<u32>,         // 0x350
    _padding18:                       [u32; 3],
    pub lvt_lint1:                    Volatile<u32>,         // 0x360
    _padding19:                       [u32; 3],
    pub lvt_error:                    Volatile<u32>,         // 0x370
    _padding20:                       [u32; 3],
    pub timer_initial_count:          Volatile<u32>,         // 0x380
    _padding21:                       [u32; 3],
    pub timer_current_count:          ReadOnly<u32>,         // 0x390
    _padding22:                       [u32; 3 + 4*4],
    pub timer_divide:                 Volatile<u32>,         // 0x3E0
    _padding23:                       [u32; 3 + 1*4],
    // ends at 0x400
}

#[repr(C)]
pub struct RegisterArray {
    reg0:                             ReadOnly<u32>,
    _padding0:                        [u32; 3],
    reg1:                             ReadOnly<u32>,
    _padding1:                        [u32; 3],
    reg2:                             ReadOnly<u32>,
    _padding2:                        [u32; 3],
    reg3:                             ReadOnly<u32>,
    _padding3:                        [u32; 3],
    reg4:                             ReadOnly<u32>,
    _padding4:                        [u32; 3],
    reg5:                             ReadOnly<u32>,
    _padding5:                        [u32; 3],
    reg6:                             ReadOnly<u32>,
    _padding6:                        [u32; 3],
    reg7:                             ReadOnly<u32>,
    _padding7:                        [u32; 3],
}


/// This structure represents a single APIC in the system, there is one per core. 
pub struct LocalApic {
    /// Only exists for xapic, should be None for x2apic systems.
    pub regs: Option<BoxRefMut<MappedPages, ApicRegisters>>,
    /// The processor id of this APIC.
    pub processor: u8,
    /// The APIC system id of this APIC.
    pub apic_id: u8,
    /// Whether this `LocalApic` is the bootstrap processor (the first processor to boot up).
    pub is_bsp: bool,
}
use core::fmt;
impl fmt::Debug for LocalApic {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "LocalApic {{processor: {}, apic_id: {}, is_bsp: {}}}",
                self.processor, self.apic_id, self.is_bsp
            )
        }
}

impl LocalApic {
    /// This MUST be invoked from the AP core itself when it is booting up.
    /// The BSP cannot invoke this for other APs (it can only invoke it for itself).
    pub fn new(active_table: &mut ActivePageTable, processor: u8, apic_id: u8, is_bsp: bool, nmi_lint: u8, nmi_flags: u16) 
        -> Result<LocalApic, &'static str>
    {

		let mut lapic = LocalApic {
            regs: None, // None by default (for x2apics). if xapic, it will be set to Some later
            processor: processor,
            apic_id: apic_id,
            is_bsp: is_bsp,
		};

        if is_bsp {
            BSP_PROCESSOR_ID.call_once( || apic_id); 
        }


        if has_x2apic() { 
            lapic.enable_x2apic();
            lapic.init_timer_x2apic();
        } 
        else { 
            // offset into the apic_mapped_page is always 0, regardless of the physical address
            let apic_regs = BoxRefMut::new(Box::new(map_apic(active_table)?)).try_map_mut(|mp| mp.as_type_mut::<ApicRegisters>(0))?;
            lapic.regs = Some(apic_regs);
            lapic.enable_apic()?;
            lapic.init_timer()?;
        }

        lapic.set_nmi(nmi_lint, nmi_flags)?;
        info!("Found new processor core ({:?})", lapic);
		Ok(lapic)
    }

    /// enables the spurious interrupt vector, which enables the APIC itself.
    fn enable_apic(&mut self) -> Result<(), &'static str> {
        assert!(!has_x2apic(), "an x2apic system must not use enable_apic(), it should use enable_x2apic() instead.");

        let is_bsp = rdmsr(IA32_APIC_BASE) & IA32_APIC_BASE_MSR_IS_BSP == IA32_APIC_BASE_MSR_IS_BSP;
        // globally enable the apic by setting the xapic_enable bit
        unsafe { wrmsr(IA32_APIC_BASE, rdmsr(IA32_APIC_BASE) | IA32_APIC_XAPIC_ENABLE); }
        info!("LAPIC ID {:#x}, version: {:#x}, is_bsp: {}", self.id(), self.version(), is_bsp);
        if is_bsp {
            INTERRUPT_CHIP.store(InterruptChip::APIC, Ordering::Release);
        }

        // init APIC to a clean state
        // see this: http://wiki.osdev.org/APIC#Logical_Destination_Mode
        if let Some(ref mut regs) = self.regs {
            regs.destination_format.write(0xFFFF_FFFF);
            info!("enable_apic(): LDR = {:#X}", regs.destination_format.read());
            regs.lvt_timer.write(APIC_DISABLE);
            regs.lvt_perf_monitor.write(APIC_NMI);
            regs.lvt_lint0.write(APIC_DISABLE);
            regs.lvt_lint1.write(APIC_DISABLE);
            regs.task_priority.write(0);

            // set bit 8 to allow receiving interrupts (still need to "sti")
            regs.spurious_interrupt_vector.write(APIC_SPURIOUS_INTERRUPT_VECTOR | APIC_SW_ENABLE);   
            Ok(())         
        }
        else {
            error!("enable_apic(): FATAL ERROR: regs (ApicRegisters) were None! Were they initialized right?");
            Err("enable_apic(): FATAL ERROR: regs (ApicRegisters) were None! Were they initialized right?")
        }
    }


    fn enable_x2apic(&mut self) {
        assert!(has_x2apic(), "an apic/xapic system must not use enable_x2apic(), it should use enable_apic() instead.");

        let is_bsp = rdmsr(IA32_APIC_BASE) & IA32_APIC_BASE_MSR_IS_BSP == IA32_APIC_BASE_MSR_IS_BSP;
        // globally enable the x2apic by setting both the x2apic and xapic enable bits
        unsafe { wrmsr(IA32_APIC_BASE, rdmsr(IA32_APIC_BASE) | IA32_APIC_XAPIC_ENABLE | IA32_APIC_X2APIC_ENABLE); }
        info!("LAPIC x2 ID {:#x}, version: {:#x}, is_bsp: {}", self.id(), self.version(), is_bsp);
        if is_bsp {
            INTERRUPT_CHIP.store(InterruptChip::X2APIC, Ordering::Release);
        }


        // init x2APIC to a clean state, just as in enable_apic() above 
        // Note: in x2apic, there is not DFR reg because only cluster mode is enabled; there is no flat logical mode
        // Note: in x2apic, the IA32_X2APIC_LDR is read-only.
        let ldr = rdmsr(IA32_X2APIC_LDR);
        let cluster_id = (ldr >> 16) & 0xFFFF; // highest 16 bits
        let logical_id = ldr & 0xFFFF; // lowest 16 bits
        info!("x2LAPIC ID {:#x}, version {:#X}, (cluster {:#X} logical {:#X}), is_bsp: {}", self.id(), self.version(), cluster_id, logical_id, is_bsp);
        // NOTE: we're not yet using logical or cluster mode APIC addressing, but rather only physical APIC addressing
        
        unsafe {
            wrmsr(IA32_X2APIC_LVT_TIMER,  APIC_DISABLE as u64);
            wrmsr(IA32_X2APIC_LVT_PMI,    APIC_NMI as u64);
            wrmsr(IA32_X2APIC_LVT_LINT0,  APIC_DISABLE as u64);
            wrmsr(IA32_X2APIC_LVT_LINT1,  APIC_DISABLE as u64);
            wrmsr(IA32_X2APIC_TPR,        0);
            
            // set bit 8 to start receiving interrupts (still need to "sti")
            wrmsr(IA32_X2APIC_SIVR, (APIC_SPURIOUS_INTERRUPT_VECTOR | APIC_SW_ENABLE) as u64); 
        }
    }


    /// Returns the number of APIC ticks that occurred during the given number of `microseconds`.
    fn calibrate_apic_timer(&mut self, microseconds: u32) -> Result<u32, &'static str> {
        assert!(!has_x2apic(), "an x2apic system must not use calibrate_apic_timer(), it should use calibrate_apic_timer_x2() instead.");
        
        if let Some(ref mut regs) = self.regs {
            regs.timer_divide.write(3); // set divide value to 16
            const INITIAL_COUNT: u32 = 0xFFFF_FFFF; // the max count, since we're counting down
            
            regs.timer_initial_count.write(INITIAL_COUNT); // set counter to max value

            // wait or the given period using the PIT clock
            pit_wait(microseconds).unwrap();

            regs.lvt_timer.write(APIC_DISABLE); // stop apic timer
            let after = regs.timer_current_count.read();
            let elapsed = INITIAL_COUNT - after;
            Ok(elapsed)
        }
        else {
            error!("calibrate_apic_timer(): FATAL ERROR: regs (ApicRegisters) were None! Were they initialized right?");
            Err("calibrate_apic_timer(): FATAL ERROR: regs (ApicRegisters) were None! Were they initialized right?")
        }
    }


    /// Returns the number of APIC ticks that occurred during the given number of `microseconds`.
    fn calibrate_x2apic_timer(&mut self, microseconds: u32) -> u64 {
        assert!(has_x2apic(), "an apic/xapic system must not use calibrate_x2apic_timer(), it should use calibrate_apic_timer_x2() instead.");
        unsafe { wrmsr(IA32_X2APIC_DIV_CONF, 3); } // set divide value to 16
        const INITIAL_COUNT: u64 = 0xFFFF_FFFF;
        
        unsafe { wrmsr(IA32_X2APIC_INIT_COUNT, INITIAL_COUNT); } // set counter to max value

        // wait or the given period using the PIT clock
        pit_wait(microseconds).unwrap();

        unsafe { wrmsr(IA32_X2APIC_LVT_TIMER, APIC_DISABLE as u64); } // stop apic timer
        let after = rdmsr(IA32_X2APIC_CUR_COUNT);
        let elapsed = INITIAL_COUNT - after;
        elapsed
    }


    fn init_timer(&mut self) -> Result<(), &'static str> {
        assert!(!has_x2apic(), "an x2apic system must not use init_timer(), it should use init_timer_x2apic() instead.");
        let apic_period = if cfg!(feature = "apic_timer_fixed") {
            info!("apic_timer_fixed config: overriding APIC timer period to {}", 0x10000);
            0x10000 // for bochs, which doesn't do apic periods right
        } else {
            self.calibrate_apic_timer(CONFIG_TIMESLICE_PERIOD_MICROSECONDS)?
        };
        trace!("APIC {}, timer period count: {}({:#X})", self.apic_id, apic_period, apic_period);

        if let Some(ref mut regs) = self.regs {
            regs.timer_divide.write(3); // set divide value to 16 ( ... how does 3 => 16 )
            // map APIC timer to an interrupt handler in the IDT
            regs.lvt_timer.write(0x22 | APIC_TIMER_PERIODIC); 
            regs.timer_initial_count.write(apic_period); 

            regs.lvt_thermal.write(0);
            regs.lvt_error.write(0);

            // os dev wiki guys say that setting this again as a last step helps on some strange hardware.
            regs.timer_divide.write(3);

            Ok(())
        }
        else {
            error!("calibrate_apic_timer(): FATAL ERROR: regs (ApicRegisters) were None! Were they initialized right?");
            Err("calibrate_apic_timer(): FATAL ERROR: regs (ApicRegisters) were None! Were they initialized right?")
        }
    }


    fn init_timer_x2apic(&mut self) {
        assert!(has_x2apic(), "an apic/xapic system must not use init_timerx2(), it should use init_timer() instead.");
        let x2apic_period = if cfg!(feature = "apic_timer_fixed") {
            info!("apic_timer_fixed config: overriding X2APIC timer period to {}", 0x10000);
            0x10000 // for bochs, which doesn't do x2apic periods right
        } else {
            self.calibrate_x2apic_timer(CONFIG_TIMESLICE_PERIOD_MICROSECONDS)
        };
        trace!("X2APIC {}, timer period count: {}({:#X})", self.apic_id, x2apic_period, x2apic_period);

        unsafe {
            wrmsr(IA32_X2APIC_DIV_CONF, 3); // set divide value to 16 ( ... how does 3 => 16 )
            
            // map X2APIC timer to an interrupt handler in the IDT, which we currently use IRQ 0x22 for
            wrmsr(IA32_X2APIC_LVT_TIMER, 0x22 | APIC_TIMER_PERIODIC as u64); 
            wrmsr(IA32_X2APIC_INIT_COUNT, x2apic_period); 

            wrmsr(IA32_X2APIC_LVT_THERMAL, 0);
            wrmsr(IA32_X2APIC_ESR, 0);

            // os dev wiki guys say that setting this again as a last step helps on some strange hardware.
            wrmsr(IA32_X2APIC_DIV_CONF, 3);
        }
    }

    
    pub fn id(&self) -> u8 {
        let id: u8 = if has_x2apic() {
            rdmsr(IA32_X2APIC_APICID) as u32 as u8
        } else {
            let raw = self.regs.as_ref().expect("ApicRegisters").lapic_id.read();
            (raw >> 24) as u8
        };
        assert!(id == self.apic_id, "LocalApic::id() {} wasn't the same as given apic_id {}!", id, self.apic_id);
        id
    }

    pub fn version(&self) -> u32 {
        if has_x2apic() {
            (rdmsr(IA32_X2APIC_VERSION) & 0xFFFF_FFFF) as u32
        } else {
            self.regs.as_ref().expect("ApicRegisters").lapic_version.read()
        }
    }

    pub fn error(&self) -> u32 {
        let raw = if has_x2apic() {
            (rdmsr(IA32_X2APIC_ESR) & 0xFFFF_FFFF) as u32
        } else {
            self.regs.as_ref().expect("ApicRegisters").error_status.read()
        };
        raw & 0x0000_00F0
    }

    pub fn clear_error(&mut self) {
        if has_x2apic() {
            unsafe { wrmsr(IA32_X2APIC_ESR, 0); }
        } else {
            // a no-op, since apic/xapic cannot write to the error status register
        }
    }

    pub fn icr(&self) -> u64 {
        if has_x2apic() {
            rdmsr(IA32_X2APIC_ICR)
        } else {
            let high = self.regs.as_ref().expect("ApicRegisters").interrupt_command_high.read();
            let low  = self.regs.as_ref().expect("ApicRegisters").interrupt_command_low.read();
            ((high as u64) << 32) | (low as u64)
        }
    }

    pub fn set_icr(&mut self, value: u64) {
        if has_x2apic() {
            unsafe { wrmsr(IA32_X2APIC_ICR, value); }
        } else {
            const ICR_DELIVERY_STATUS: u32 = 1 << 12;
            while self.regs.as_ref().expect("ApicRegisters").interrupt_command_low.read() & ICR_DELIVERY_STATUS == ICR_DELIVERY_STATUS {} // wait until ready
            let high = (value >> 32) as u32;
            self.regs.as_mut().expect("ApicRegisters").interrupt_command_high.write(high); // sets part of ICR register, but doesn't yet issue the IPI
            let low = value as u32;
            self.regs.as_mut().expect("ApicRegisters").interrupt_command_low.write(low); // this actually issues the IPI
            while self.regs.as_ref().expect("ApicRegisters").interrupt_command_low.read() & ICR_DELIVERY_STATUS == ICR_DELIVERY_STATUS {} // wait until finished
        }
    }

    /// Send an IPI to the cores specified by the given destination
    pub fn send_ipi(&mut self, irq: u8, destination: LapicIpiDestination) {
        const NORMAL_IPI_ICR: u64 = 0x4000;
        
        let dest = destination.as_icr_value();
        let icr = NORMAL_IPI_ICR | (irq as u64) | dest;

        // trace!("send_ipi(): setting icr value to {:#X}", icr);
        self.set_icr(icr);
    }


    /// Send a NMI IPI to the cores specified by the given destination
    pub fn send_nmi_ipi(&mut self, destination: LapicIpiDestination) {
        const NORMAL_IPI_ICR: u64 = 0x4000;
        const NMI_DELIVERY_MODE: u64 = 0b100 << 8;
        
        let dest = destination.as_icr_value();
        let icr = NORMAL_IPI_ICR | NMI_DELIVERY_MODE | dest;

        // trace!("send_ipi(): setting icr value to {:#X}", icr);
        self.set_icr(icr);
    }



    /// Sends an IPI to all other cores (except me) to trigger 
    /// a TLB flush of the given `VirtualAddress`
    pub fn send_tlb_shootdown_ipi(&mut self, vaddr: VirtualAddress) {

        // temporary page is not shared across cores
        use kernel_config::memory::{TEMPORARY_PAGE_VIRT_ADDR, PAGE_SIZE};
        const TEMPORARY_PAGE_FRAME: usize = TEMPORARY_PAGE_VIRT_ADDR & !(PAGE_SIZE - 1);
        if vaddr == TEMPORARY_PAGE_FRAME { 
            return;
        }
        
        let core_count = get_lapics().iter().count();
        if core_count <= 1 {
            return; // skip sending IPIs if there are no other cores running
        }

        // trace!("send_tlb_shootdown_ipi(): from AP {}, vaddr: {:#X}, core_count: {}", 
        //         get_my_apic_id().unwrap_or(0xff), vaddr, core_count);
        
        // interrupts must be disabled here, because this IPI sequence must be fully synchronous with other cores,
        // and we wouldn't want this core to be interrupted while coordinating IPI responses across multiple cores.
        let _held_ints = hold_interrupts(); 

        // acquire lock
        // TODO: add timeout!!
        while TLB_SHOOTDOWN_IPI_LOCK.compare_and_swap(false, true, Ordering::SeqCst) { 
            spin_loop_hint();
        }

        TLB_SHOOTDOWN_IPI_VIRT_ADDR.store(vaddr, Ordering::Release);
        TLB_SHOOTDOWN_IPI_COUNT.store(core_count - 1, Ordering::SeqCst); // - 1 to exclude this core 

        // let's try to use NMI instead, since it will interrupt everyone forcibly and result in the fastest handling
        self.send_nmi_ipi(LapicIpiDestination::AllButMe); // send IPI to all other cores but this one
        // self.send_ipi(TLB_SHOOTDOWN_IPI_IRQ, LapicIpiDestination::AllButMe); // send IPI to all other cores but this one

        // wait for all other cores to handle this IPI
        // it must be a blocking, synchronous operation to ensure stale TLB entries don't cause problems
        // TODO: add timeout!!
        while TLB_SHOOTDOWN_IPI_COUNT.load(Ordering::SeqCst) > 0 { 
            spin_loop_hint();
        }
    
        // clear TLB shootdown data
        TLB_SHOOTDOWN_IPI_VIRT_ADDR.store(0, Ordering::Release);

        // release lock
        TLB_SHOOTDOWN_IPI_LOCK.store(false, Ordering::SeqCst); 
    }


    pub fn eoi(&mut self) {
        // 0 is the only valid value to write to the EOI register/msr, others cause General Protection Fault
        unsafe {
            if has_x2apic() {
                wrmsr(IA32_X2APIC_EOI, 0); 
            } else {
                self.regs.as_mut().expect("ApicRegisters").eoi.write(0);
            }
        }
    }


    pub fn set_ldr(&mut self, value: u32) {
        assert!(!has_x2apic(),"set_ldr(): Setting LDR MSR for x2apic is forbidden! (causes GPF)");
        let old_ldr = self.regs.as_ref().expect("ApicRegisters").destination_format.read();
        self.regs.as_mut().expect("ApicRegisters").destination_format.write(old_ldr & 0x00FF_FFFF | value);
    }

    /// Set the NonMaskableInterrupt redirect for this LocalApic.
    /// Argument `lint` can be either 0 or 1, since each local APIC has two LVT LINTs
    /// (Local Vector Table Local INTerrupts)
    pub fn set_nmi(&mut self, lint: u8, flags: u16) -> Result<(), &'static str> {
        if has_x2apic() {
            match lint {
                0 => unsafe { wrmsr(IA32_X2APIC_LVT_LINT0, (flags << 12) as u64 | APIC_NMI as u64) }, // or APIC_NMI | 0x2000 ??
                1 => unsafe { wrmsr(IA32_X2APIC_LVT_LINT1, (flags << 12) as u64 | APIC_NMI as u64) },
                _ => return Err("set_nmi(): invalid lint {}, must be 0 or 1!"),
            }
        } else {
            match lint {
                0 => self.regs.as_mut().expect("ApicRegisters").lvt_lint0.write( (flags << 12) as u32 | APIC_NMI), // or APIC_NMI | 0x2000 ??
                1 => self.regs.as_mut().expect("ApicRegisters").lvt_lint1.write( (flags << 12) as u32 | APIC_NMI),
                _ => return Err("set_nmi(): invalid lint {}, must be 0 or 1!"),
            }
        }

        Ok(())
    }


    pub fn get_isr(&self) -> (u32, u32, u32, u32, u32, u32, u32, u32) {
        if has_x2apic() {
            ( 
                rdmsr(IA32_X2APIC_ISR0) as u32, 
                rdmsr(IA32_X2APIC_ISR1) as u32,
                rdmsr(IA32_X2APIC_ISR2) as u32, 
                rdmsr(IA32_X2APIC_ISR3) as u32,
                rdmsr(IA32_X2APIC_ISR4) as u32,
                rdmsr(IA32_X2APIC_ISR5) as u32,
                rdmsr(IA32_X2APIC_ISR6) as u32,
                rdmsr(IA32_X2APIC_ISR7) as u32,
            )
        }
        else {
            let ref isr = self.regs.as_ref().expect("ApicRegisters").in_service_registers;
            (
                isr.reg0.read(),
                isr.reg1.read(),
                isr.reg2.read(),
                isr.reg3.read(),
                isr.reg4.read(),
                isr.reg5.read(),
                isr.reg6.read(),
                isr.reg7.read(),
            )
        }
    }


    pub fn get_irr(&self) -> (u32, u32, u32, u32, u32, u32, u32, u32) {
        if has_x2apic() {
            ( 
                rdmsr(IA32_X2APIC_IRR0) as u32, 
                rdmsr(IA32_X2APIC_IRR1) as u32,
                rdmsr(IA32_X2APIC_IRR2) as u32, 
                rdmsr(IA32_X2APIC_IRR3) as u32,
                rdmsr(IA32_X2APIC_IRR4) as u32,
                rdmsr(IA32_X2APIC_IRR5) as u32,
                rdmsr(IA32_X2APIC_IRR6) as u32,
                rdmsr(IA32_X2APIC_IRR7) as u32,
            )
        }
        else {
            let ref irr = self.regs.as_ref().expect("ApicRegisters").interrupt_request_registers;
            (
                irr.reg0.read(),
                irr.reg1.read(),
                irr.reg2.read(),
                irr.reg3.read(),
                irr.reg4.read(),
                irr.reg5.read(),
                irr.reg6.read(),
                irr.reg7.read(),
            )
        }
    }
}




/// The IRQ number used for IPIs
pub const TLB_SHOOTDOWN_IPI_IRQ: u8 = 0x40;
/// The virtual address used for TLB shootdown IPIs
pub static TLB_SHOOTDOWN_IPI_VIRT_ADDR: AtomicUsize = AtomicUsize::new(0); 
/// The number of remaining cores that still need to handle the curerent TLB shootdown IPI
pub static TLB_SHOOTDOWN_IPI_COUNT: AtomicUsize = AtomicUsize::new(0); 
/// The lock that makes sure only one set of TLB shootdown IPIs is concurrently happening
pub static TLB_SHOOTDOWN_IPI_LOCK: AtomicBool = AtomicBool::new(false);


/// Broadcasts TLB shootdown IPI to all other AP cores.
/// Do not invoke this directly, but rather pass it as a callback to the memory subsystem,
/// which will invoke it as needed (on remap/unmap operations).
pub fn broadcast_tlb_shootdown(vaddr: VirtualAddress) {
    if let Some(my_lapic) = get_my_apic() {
        // trace!("broadcast_tlb_shootdown(): (AP {}) sending tlb shootdown ipi for vaddr {:#X}", my_lapic.apic_id, vaddr);
        my_lapic.write().send_tlb_shootdown_ipi(vaddr);
    }
}


/// Handles a TLB shootdown ipi by flushing the VirtualAddress 
/// currently stored in TLB_SHOOTDOWN_IPI_VIRT_ADDR.
/// DO not invoke this directly, it will be called by an IPI interrupt handler.
pub fn handle_tlb_shootdown_ipi(vaddr: VirtualAddress) {
    // let apic_id = get_my_apic_id().unwrap_or(0xFF);
    // trace!("handle_tlb_shootdown_ipi(): (AP {}) flushing vaddr {:#X}", apic_id, vaddr);

    x86_64::instructions::tlb::flush(x86_64::VirtualAddress(vaddr));
    TLB_SHOOTDOWN_IPI_COUNT.fetch_sub(1, Ordering::SeqCst);
}
