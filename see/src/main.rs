#![no_std]
#![no_main]
#![feature(naked_functions, asm_sym, asm_const)]

mod execute;
mod extensions;
mod hart_csr_utils;

#[macro_use] // for print
extern crate rustsbi;

use core::{arch::asm, ops::Range, panic::PanicInfo};

const RAM_BASE: usize = 0x4000_0000;
const SUPERVISOR_OFFSET: usize = 0x20_0000;
const SUPERVISOR_ENTRY: usize = RAM_BASE + SUPERVISOR_OFFSET;

/// 特权软件信息。
struct Supervisor {
    start_addr: usize,
    opaque: usize,
}

#[naked]
#[no_mangle]
#[link_section = ".head.jump"]
unsafe extern "C" fn head_jump() -> ! {
    asm!(
        ".option push",
        ".option rvc",
        "c.j    0x0c",
        ".option pop",
        options(noreturn)
    )
}

#[no_mangle]
#[link_section = ".head.info"]
static mut PAYLOAD: PayloadInfo = PayloadInfo {
    total_size: 0,
    dtb_offset: 0,
};

/// 入口。
///
/// 1. 关中断
/// 2. 设置启动栈
/// 3. 跳转到 rust 入口函数
///
/// # Safety
///
/// 裸函数。
#[naked]
#[no_mangle]
#[link_section = ".text.entry"]
unsafe extern "C" fn entry() -> ! {
    const STACK_SIZE: usize = 4096;
    #[link_section = ".bss.uninit"]
    static mut STACK: [u8; STACK_SIZE] = [0; STACK_SIZE];

    asm!(
        "
            csrw mie,  zero
            la    sp, {stack}
            li    t0, {stack_size}
            add   sp,  sp, t0
            call {rust_main}
        1:  wfi
            j     1b
        ",
        stack      =   sym STACK,
        stack_size = const STACK_SIZE,
        rust_main  =   sym rust_main,
        options(noreturn)
    )
}

extern "C" fn rust_main() {
    use execute::execute_supervisor;

    extern "C" {
        static mut sbss: u64;
        static mut ebss: u64;
    }
    unsafe { r0::zero_bss(&mut sbss, &mut ebss) };

    extensions::init();

    let payload = unsafe { PAYLOAD };
    let dtb_addr = SUPERVISOR_ENTRY + payload.dtb_offset as usize;
    println!("{payload:#x?}");

    let board_info = device_tree(dtb_addr);
    let mem = board_info
        .as_ref()
        .map_or(RAM_BASE..RAM_BASE + (512 << 20), |i| i.mem.clone());
    let dtb = board_info.as_ref().map_or(0..0, |i| i.dtb.clone());
    print!(
        "
[rustsbi] RustSBI version {ver_sbi}, adapting to RISC-V SBI v1.0.0
{logo}
[rustsbi] Implementation     : RustSBI-D1 Version {ver_impl}
[rustsbi] Platform Name      : {model}
[rustsbi] Platform SMP       : 1
[rustsbi] Platform Memory    : {mem:#x?}
[rustsbi] Boot HART          : 0
[rustsbi] Device Tree Region : {dtb:#x?}
[rustsbi] Firmware Address   : {firmware:#x}
[rustsbi] Supervisor Address : {SUPERVISOR_ENTRY:#x}
",
        model = board_info
            .as_ref()
            .map_or("sun20iw1p1", |i| i.model.as_str()),
        ver_sbi = rustsbi::VERSION,
        logo = rustsbi::logo(),
        ver_impl = env!("CARGO_PKG_VERSION"),
        firmware = head_jump as usize,
    );

    set_pmp(mem.clone());
    hart_csr_utils::print_pmps();

    let plic = unsafe { &*hal::pac::PLIC::ptr() };
    use hal::pac::plic::ctrl::CTRL_A;
    plic.ctrl.write(|w| w.ctrl().variant(CTRL_A::MS));

    let dtb_addr = if !dtb.is_empty() {
        const PAGE: usize = 1 << 12;
        let start = (mem.start + mem.len().min(1 << 30)) - ((dtb.len() + PAGE - 1) / PAGE) * PAGE;
        let src = dtb.start as *const u8;
        let dst = start as *mut u8;
        unsafe { dst.copy_from(src, dtb.len()) };
        start
    } else {
        0
    };

    println!("execute_supervisor at {SUPERVISOR_ENTRY:#x} with a1 = {dtb_addr:#x}");
    execute_supervisor(Supervisor {
        start_addr: SUPERVISOR_ENTRY,
        opaque: dtb_addr,
    })
}

/// 设置 PMP。
fn set_pmp(mem: core::ops::Range<usize>) {
    use riscv::register::{pmpaddr0, pmpaddr1, pmpaddr2, pmpaddr3, pmpcfg0, Permission, Range};
    unsafe {
        pmpcfg0::set_pmp(0, Range::OFF, Permission::NONE, false);
        pmpaddr0::write(0);
        // 外设
        pmpcfg0::set_pmp(1, Range::TOR, Permission::RW, false);
        pmpaddr1::write(mem.start >> 2);
        // SBI
        pmpcfg0::set_pmp(2, Range::TOR, Permission::NONE, false);
        pmpaddr2::write(SUPERVISOR_ENTRY >> 2);
        //主存
        pmpcfg0::set_pmp(3, Range::TOR, Permission::RWX, false);
        pmpaddr3::write(mem.end >> 2);
    }
}

/// 从设备树采集的板信息。
struct BoardInfo {
    pub dtb: Range<usize>,
    pub model: StringInline<128>,
    pub mem: Range<usize>,
}

/// 在栈上存储有限长度字符串。
struct StringInline<const N: usize>(usize, [u8; N]);

impl<const N: usize> StringInline<N> {
    pub fn as_str(&self) -> &str {
        unsafe { core::str::from_utf8_unchecked(&self.1[..self.0]) }
    }
}

fn device_tree(addr: usize) -> Option<BoardInfo> {
    use dtb_walker::{Dtb, DtbObj, HeaderError::*, Property, WalkOperation::*};

    let dtb = unsafe {
        match Dtb::from_raw_parts_filtered(addr as _, |e| matches!(e, LastCompVersion(16))) {
            Ok(dtb) => dtb,
            Err(e) => {
                println!("Dtb not detected at {addr:#x}: {e:?}");
                return None;
            }
        }
    };
    let mut any = false;
    let mut ans = BoardInfo {
        dtb: addr..addr,
        model: StringInline(0, [0u8; 128]),
        mem: 0..0,
    };
    ans.dtb.end += dtb.total_size();
    dtb.walk(|path, obj| match obj {
        DtbObj::SubNode { name } => {
            if path.is_root() && name.starts_with("memory") {
                StepInto
            } else {
                StepOver
            }
        }
        DtbObj::Property(Property::Model(model)) if path.is_root() => {
            ans.model.0 = model.as_bytes().len();
            ans.model.1[..ans.model.0].copy_from_slice(model.as_bytes());
            if any {
                Terminate
            } else {
                any = true;
                StepOver
            }
        }
        DtbObj::Property(Property::Reg(mut reg)) if path.last().starts_with("memory") => {
            ans.mem = reg.next().unwrap();
            if any {
                Terminate
            } else {
                any = true;
                StepOut
            }
        }
        DtbObj::Property(_) => StepOver,
    });
    Some(ans)
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct PayloadInfo {
    total_size: u32,
    dtb_offset: u32,
}

#[cfg_attr(not(test), panic_handler)]
fn panic(info: &PanicInfo) -> ! {
    println!("{info}");
    loop {
        core::hint::spin_loop();
    }
}

#[inline(always)]
unsafe fn set_mtvec(trap_handler: usize) {
    use riscv::register::mtvec;
    mtvec::write(trap_handler, mtvec::TrapMode::Direct);
}