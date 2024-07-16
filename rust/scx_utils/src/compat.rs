// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2.

use anyhow::{anyhow, bail, Context, Result};
use libbpf_rs::libbpf_sys::*;
use std::ffi::c_void;
use std::ffi::CStr;
use std::ffi::CString;
use std::mem::size_of;
use std::slice::from_raw_parts;

lazy_static::lazy_static! {
    pub static ref SCX_OPS_SWITCH_PARTIAL: u64 =
    read_enum("scx_ops_flags", "SCX_OPS_SWITCH_PARTIAL").unwrap_or(0);
}

fn load_vmlinux_btf() -> &'static mut btf {
    let btf = unsafe { btf__load_vmlinux_btf() };
    if btf.is_null() {
        panic!("btf__load_vmlinux_btf() returned NULL");
    }
    unsafe { &mut *btf }
}

lazy_static::lazy_static! {
    static ref VMLINUX_BTF: &'static mut btf = load_vmlinux_btf();
}

fn btf_kind(t: &btf_type) -> u32 {
    (t.info >> 24) & 0x1f
}

fn btf_vlen(t: &btf_type) -> u32 {
    t.info & 0xffff
}

fn btf_type_plus_1(t: &btf_type) -> *const c_void {
    let ptr_val = t as *const btf_type as usize;
    (ptr_val + size_of::<btf_type>()) as *const c_void
}

fn btf_enum(t: &btf_type) -> &[btf_enum] {
    let ptr = btf_type_plus_1(t);
    unsafe { from_raw_parts(ptr as *const btf_enum, btf_vlen(t) as usize) }
}

fn btf_enum64(t: &btf_type) -> &[btf_enum64] {
    let ptr = btf_type_plus_1(t);
    unsafe { from_raw_parts(ptr as *const btf_enum64, btf_vlen(t) as usize) }
}

fn btf_members(t: &btf_type) -> &[btf_member] {
    let ptr = btf_type_plus_1(t);
    unsafe { from_raw_parts(ptr as *const btf_member, btf_vlen(t) as usize) }
}

fn btf_name_str_by_offset(btf: &btf, name_off: u32) -> Result<&str> {
    let n = unsafe { btf__name_by_offset(btf, name_off) };
    if n.is_null() {
        bail!("btf__name_by_offset() returned NULL");
    }
    Ok(unsafe { CStr::from_ptr(n) }
        .to_str()
        .with_context(|| format!("Failed to convert {:?} to string", n))?)
}

pub fn read_enum(type_name: &str, name: &str) -> Result<u64> {
    let btf: &btf = *VMLINUX_BTF;

    let type_name = CString::new(type_name).unwrap();
    let tid = unsafe { btf__find_by_name(btf, type_name.as_ptr()) };
    if tid < 0 {
        bail!("type {:?} doesn't exist, ret={}", type_name, tid);
    }

    let t = unsafe { btf__type_by_id(btf, tid as _) };
    if t.is_null() {
        bail!("btf__type_by_id({}) returned NULL", tid);
    }
    let t = unsafe { &*t };

    match btf_kind(t) {
        BTF_KIND_ENUM => {
            for e in btf_enum(t).iter() {
                if btf_name_str_by_offset(btf, e.name_off)? == name {
                    return Ok(e.val as u64);
                }
            }
        }
        BTF_KIND_ENUM64 => {
            for e in btf_enum64(t).iter() {
                if btf_name_str_by_offset(btf, e.name_off)? == name {
                    return Ok(((e.val_hi32 as u64) << 32) | (e.val_lo32) as u64);
                }
            }
        }
        _ => (),
    }

    Err(anyhow!("{:?} doesn't exist in {:?}", name, type_name))
}

pub fn struct_has_field(type_name: &str, field: &str) -> Result<bool> {
    let btf: &btf = *VMLINUX_BTF;

    let type_name = CString::new(type_name).unwrap();
    let tid = unsafe { btf__find_by_name_kind(btf, type_name.as_ptr(), BTF_KIND_STRUCT) };
    if tid < 0 {
        bail!("type {:?} doesn't exist, ret={}", type_name, tid);
    }

    let t = unsafe { btf__type_by_id(btf, tid as _) };
    if t.is_null() {
        bail!("btf__type_by_id({}) returned NULL", tid);
    }
    let t = unsafe { &*t };

    for m in btf_members(t).iter() {
        if btf_name_str_by_offset(btf, m.name_off)? == field {
            return Ok(true);
        }
    }

    return Ok(false);
}

/// struct sched_ext_ops can change over time. If
/// compat.bpf.h::SCX_OPS_DEFINE() is used to define ops and scx_ops_load!()
/// and scx_ops_attach!() are used to load and attach it, backward
/// compatibility is automatically maintained where reasonable.
///
/// - sched_ext_ops.exit_dump_len was added later. On kernels which don't
/// support it, the value is ignored and a warning is triggered if the value
/// is requested to be non-zero.
#[macro_export]
macro_rules! scx_ops_load {
    ($skel: expr, $ops: ident, $uei: ident) => {{
        scx_utils::paste! {
            scx_utils::uei_set_size!($skel, $ops, $uei);

            let ops = $skel.struct_ops.[<$ops _mut>]();
            let has_field = scx_utils::compat::struct_has_field("sched_ext_ops", "exit_dump_len")?;
            if !has_field && ops.exit_dump_len != 0 {
                scx_utils::warn!("Kernel doesn't support setting exit dump len");
                ops.exit_dump_len = 0;
            }

            $skel.load().context("Failed to load BPF program")
        }
    }};
}

/// Must be used together with scx_ops_load!(). See there.
#[macro_export]
macro_rules! scx_ops_attach {
    ($skel: expr, $ops: ident) => {{
        $skel
            .maps
            .$ops
            .attach_struct_ops()
            .context("Failed to attach struct ops")
    }};
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_read_enum() {
        assert_eq!(super::read_enum("pid_type", "PIDTYPE_TGID").unwrap(), 1);
    }

    #[test]
    fn test_struct_has_field() {
        assert!(super::struct_has_field("task_struct", "flags").unwrap());
        assert!(!super::struct_has_field("task_struct", "NO_SUCH_FIELD").unwrap());
        assert!(super::struct_has_field("NO_SUCH_STRUCT", "NO_SUCH_FIELD").is_err());
    }
}
