use std;
use std::ptr::null;
use libc;

use baseline;
use baseline::fct::BailoutInfo;
use baseline::map::CodeData;
use cpu;
use ctxt::{Context, CTXT, get_ctxt};
use execstate::ExecState;
use object::{Handle, Obj};
use os_cpu::*;
use stacktrace::{handle_exception, get_stacktrace};

#[cfg(target_family = "windows")]
use winapi::winnt::EXCEPTION_POINTERS;

#[cfg(target_family = "unix")]
pub fn register_signals(ctxt: &Context) {
    unsafe {
        let ptr = ctxt as *const Context as *const u8;
        CTXT = Some(ptr);

        let mut sa: libc::sigaction = std::mem::uninitialized();

        sa.sa_sigaction = handler as usize;
        libc::sigemptyset(&mut sa.sa_mask as *mut libc::sigset_t);
        sa.sa_flags = libc::SA_SIGINFO;

        if libc::sigaction(libc::SIGSEGV,
                           &sa as *const libc::sigaction,
                           0 as *mut libc::sigaction) == -1 {
            libc::perror("sigaction for SIGSEGV failed".as_ptr() as *const libc::c_char);
        }

        if libc::sigaction(libc::SIGILL,
                           &sa as *const libc::sigaction,
                           0 as *mut libc::sigaction) == -1 {
            libc::perror("sigaction for SIGILL failed".as_ptr() as *const libc::c_char);
        }
    }
}

#[cfg(target_family = "windows")]
pub fn register_signals(ctxt: &Context) {
    use kernel32::AddVectoredExceptionHandler;

    unsafe {
        AddVectoredExceptionHandler(1, Some(handler));
    }
}

#[cfg(target_family = "windows")]
extern "system" fn handler(exception: *mut EXCEPTION_POINTERS) -> i32 {
    use winapi::excpt;

    if fault_handler(exception) {
        return excpt::ExceptionContinueExecution.0 as i32;
    }

    excpt::ExceptionContinueSearch.0 as i32
}

#[cfg(target_family = "windows")]
fn fault_handler(exception: *mut EXCEPTION_POINTERS) -> bool {
    unsafe {
        let record = (*exception).ExceptionRecord;
        let context = (*exception).ContextRecord;
    }

    false
}

#[cfg(target_family = "unix")]
fn handler(signo: libc::c_int, _: *const u8, ucontext: *const u8) {
    let mut es = read_execstate(ucontext);
    let ctxt = get_ctxt();

    if let Some(trap) = detect_trap(signo as i32, &es) {
        match trap {
            Trap::COMPILER => compile_request(ctxt, &mut es, ucontext),

            Trap::DIV0 => {
                println!("division by 0");
                let stacktrace = get_stacktrace(ctxt, &es);
                stacktrace.dump(ctxt);
                unsafe {
                    libc::_exit(101);
                }
            }

            Trap::ASSERT => {
                println!("assert failed");
                let stacktrace = get_stacktrace(ctxt, &es);
                stacktrace.dump(ctxt);
                unsafe {
                    libc::_exit(101);
                }
            }

            Trap::INDEX_OUT_OF_BOUNDS => {
                println!("array index out of bounds");
                let stacktrace = get_stacktrace(ctxt, &es);
                stacktrace.dump(ctxt);
                unsafe {
                    libc::_exit(102);
                }
            }

            Trap::NIL => {
                println!("nil check failed");
                let stacktrace = get_stacktrace(ctxt, &es);
                stacktrace.dump(ctxt);
                unsafe {
                    libc::_exit(103);
                }
            }

            Trap::THROW => {
                let handler_found = handle_exception(&mut es);

                if handler_found {
                    write_execstate(&es, ucontext as *mut u8);
                } else {
                    println!("uncaught exception");
                    unsafe {
                        libc::_exit(104);
                    }
                }
            }

            Trap::CAST => {
                println!("cast failed");
                let stacktrace = get_stacktrace(ctxt, &es);
                stacktrace.dump(ctxt);
                unsafe {
                    libc::_exit(105);
                }
            }

            Trap::UNEXPECTED => {
                println!("unexpected exception");
                let stacktrace = get_stacktrace(ctxt, &es);
                stacktrace.dump(ctxt);
                unsafe {
                    libc::_exit(106);
                }
            }
        }

        // could not recognize trap -> crash vm
    } else {
        println!("error: trap not detected (signal {}).", signo);
        println!();
        println!("{:?}", &es);
        println!();

        {
            let code_map = ctxt.code_map.lock().unwrap();
            code_map.dump(ctxt);
        }

        unsafe {
            libc::_exit(1);
        }
    }
}

fn compile_request(ctxt: &Context, es: &mut ExecState, ucontext: *const u8) {
    let data = {
        let code_map = ctxt.code_map.lock().unwrap();
        code_map.get(es.pc as *const u8)
    };

    match data {
        Some(CodeData::CompileStub) => {
            let mut sfi = cpu::sfi_from_execution_state(es);

            ctxt.use_sfi(&mut sfi, || {
                patch_fct_call(ctxt, es);
            });

            write_execstate(es, ucontext as *mut u8);
        }

        Some(CodeData::VirtCompileStub) => {
            let mut sfi = cpu::sfi_from_execution_state(es);

            ctxt.use_sfi(&mut sfi, || {
                patch_vtable_call(ctxt, es);
            });

            write_execstate(es, ucontext as *mut u8);
        }

        _ => {
            println!("error: code not found for address {:x}", es.pc);
            unsafe {
                libc::_exit(200);
            }
        }
    }
}

fn patch_vtable_call(ctxt: &Context, es: &mut ExecState) {
    let vtable_index = {
        // get return address from top of stack
        let ra = cpu::ra_from_execstate(es);

        let data = {
            let code_map = ctxt.code_map.lock().unwrap();
            code_map.get(ra as *const u8).expect("return address not found")
        };

        let fct_id = match data {
            CodeData::Fct(fct_id) => fct_id,
            _ => panic!("expected function for code")
        };

        let fct = ctxt.fcts[fct_id].borrow();
        let src = fct.src();
        let src = src.lock().unwrap();
        let jit_fct = src.jit_fct.as_ref().expect("jitted fct not found");

        let offset = ra - jit_fct.fct_ptr() as usize;
        let bailout = jit_fct.bailouts.get(offset as i32).expect("bailout info not found");

        match bailout {
            &BailoutInfo::VirtCompile(fct_id) => fct_id,
            _ => panic!("no info for virtual call found")
        }
    };

    let obj : Handle<Obj> = cpu::receiver_from_execstate(es).into();

    let vtable = obj.header().vtbl();
    let cls_id = vtable.class().id;
    let cls = ctxt.classes[cls_id].borrow();

    let mut fct_ptr = null();

    for &fct_id in &cls.methods {
        let fct = ctxt.fcts[fct_id].borrow();

        if Some(vtable_index) == fct.vtable_index {
            fct_ptr = baseline::generate(ctxt, fct_id);
            break;
        }
    }

    let methodtable = vtable.table_mut();
    methodtable[vtable_index as usize] = fct_ptr as usize;

    // execute fct call again
    es.pc = fct_ptr as usize;
}

pub fn patch_fct_call(ctxt: &Context, es: &mut ExecState) {
    // get return address from top of stack
    let ra = cpu::ra_from_execstate(es);

    let data = {
        let code_map = ctxt.code_map.lock().unwrap();
        code_map.get(ra as *const u8).expect("return address not found")
    };

    let fct_id = match data {
        CodeData::Fct(fct_id) => fct_id,
        _ => panic!("expected function for code")
    };

    let (fct_id, disp) = {
        let fct = ctxt.fcts[fct_id].borrow();
        let src = fct.src();
        let src = src.lock().unwrap();
        let jit_fct = src.jit_fct.as_ref().expect("jitted fct not found");

        let offset = ra - jit_fct.fct_ptr() as usize;
        let bailout = jit_fct.bailouts.get(offset as i32).expect("bailout info not found");

        match bailout {
            &BailoutInfo::Compile(fct_id, disp) => (fct_id, disp),
            _ => panic!("no info for direct call found")
        }
    };

    let fct_ptr = baseline::generate(ctxt, fct_id);
    let fct_addr: *mut usize = (ra as isize - disp as isize) as *mut _;

    // write function pointer
    unsafe {
        *fct_addr = fct_ptr as usize;
    }

    // execute fct call again
    es.pc = fct_ptr as usize;
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Trap {
    COMPILER,
    DIV0,
    ASSERT,
    INDEX_OUT_OF_BOUNDS,
    NIL,
    THROW,
    CAST,
    UNEXPECTED,
}

impl Trap {
    pub fn int(self) -> u32 {
        match self {
            Trap::COMPILER => 0,
            Trap::DIV0 => 1,
            Trap::ASSERT => 2,
            Trap::INDEX_OUT_OF_BOUNDS => 3,
            Trap::NIL => 4,
            Trap::THROW => 5,
            Trap::CAST => 6,
            Trap::UNEXPECTED => 7,
        }
    }

    pub fn from(value: u32) -> Option<Trap> {
        match value {
            0 => Some(Trap::COMPILER),
            1 => Some(Trap::DIV0),
            2 => Some(Trap::ASSERT),
            3 => Some(Trap::INDEX_OUT_OF_BOUNDS),
            4 => Some(Trap::NIL),
            5 => Some(Trap::THROW),
            6 => Some(Trap::CAST),
            7 => Some(Trap::UNEXPECTED),
            _ => None,
        }
    }
}
